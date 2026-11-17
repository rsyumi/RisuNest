use super::*;
pub(super) mod gui;
mod native;
use ::windows::core::{Interface, BSTR, HRESULT};
use ::windows::Win32::Foundation::{E_ACCESSDENIED, E_FAIL, VARIANT_BOOL};
use ::windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use ::windows::Win32::System::TaskScheduler::*;
use ::windows::Win32::System::Variant::VARIANT;
use native::{At, Failure, NativeResult};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

const HELPER_TASK_DESCRIPTION: &str = "RisuNest update helper";

fn helper_task_prefix(root: &Path) -> String {
    format!("{}-update-helper-", instance_name(root))
}

fn valid_helper_task(root: &Path, task_name: &str) -> bool {
    let prefix = helper_task_prefix(root);
    task_name.starts_with(&prefix)
        && risunest_sync_server::identity::is_uuid_v4(&task_name[prefix.len()..])
}

fn quote_argument(value: &str) -> String {
    if !value.is_empty()
        && !value
            .chars()
            .any(|value| value.is_whitespace() || value == '"')
    {
        return value.into();
    }
    let mut quoted = String::from("\"");
    let mut backslashes = 0usize;
    for character in value.chars() {
        if character == '\\' {
            backslashes += 1;
        } else {
            if character == '"' {
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
            } else {
                quoted.push_str(&"\\".repeat(backslashes));
            }
            backslashes = 0;
            quoted.push(character);
        }
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

fn connect() -> NativeResult<(ITaskService, ITaskFolder)> {
    unsafe {
        let service: ITaskService =
            CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER).at("connect")?;
        let empty = VARIANT::default();
        service
            .Connect(&empty, &empty, &empty, &empty)
            .at("connect")?;
        let folder = service.GetFolder(&BSTR::from("\\")).at("connect")?;
        Ok((service, folder))
    }
}

fn get_task(folder: &ITaskFolder, name: &str) -> NativeResult<Option<IRegisteredTask>> {
    match unsafe { folder.GetTask(&BSTR::from(name)) } {
        Ok(task) => Ok(Some(task)),
        Err(error) if matches!(error.code().0 as u32, 0x80070002 | 0x80070003) => Ok(None),
        result => result.map(Some).at("read-task"),
    }
}

fn helper_owned(task: &IRegisteredTask) -> NativeResult<bool> {
    unsafe {
        let info = task
            .Definition()
            .at("read-definition")?
            .RegistrationInfo()
            .at("read-definition")?;
        let description = native::read_string("read-definition", |value| info.Description(value))?;
        native::equal(&description, HELPER_TASK_DESCRIPTION, false)
    }
}

pub(super) fn spawn_update_helper(root: &Path, command: &mut Command) -> Result<u32> {
    let name = format!(
        "{}{}",
        helper_task_prefix(root),
        risunest_sync_server::management::discovery::request_id()
            .map_err(|_| "update-helper-start-failed")?
    );
    command.arg("--scheduled-task").arg(&name);
    let program = command
        .get_program()
        .to_str()
        .ok_or("update-helper-start-failed")?;
    let arguments = command
        .get_args()
        .map(|value| {
            value
                .to_str()
                .map(quote_argument)
                .ok_or_else(|| "update-helper-start-failed".to_owned())
        })
        .collect::<Result<Vec<_>>>()?
        .join(" ");
    let limit = if command.get_args().any(|value| value == "--owner") {
        "PT0S"
    } else {
        "PT10M"
    };
    native::on_com("update-helper-start-failed", || unsafe {
        let (service, folder) = connect()?;
        let definition = service.NewTask(0).at("definition")?;
        let info = definition.RegistrationInfo().at("definition")?;
        info.SetDescription(&BSTR::from(HELPER_TASK_DESCRIPTION))
            .at("definition")?;
        let date = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .map_err(|_| Failure {
                stage: "date",
                code: E_FAIL,
            })?;
        info.SetDate(&BSTR::from(date.as_str())).at("definition")?;
        let principal = definition.Principal().at("definition")?;
        principal
            .SetLogonType(TASK_LOGON_INTERACTIVE_TOKEN)
            .at("definition")?;
        principal.SetRunLevel(TASK_RUNLEVEL_LUA).at("definition")?;
        let settings = definition.Settings().at("definition")?;
        settings.SetEnabled(true.into()).at("definition")?;
        settings
            .SetStartWhenAvailable(true.into())
            .at("definition")?;
        settings
            .SetExecutionTimeLimit(&BSTR::from(limit))
            .at("definition")?;
        let action: IExecAction = definition
            .Actions()
            .at("definition")?
            .Create(TASK_ACTION_EXEC)
            .at("definition")?
            .cast()
            .at("definition")?;
        action.SetPath(&BSTR::from(program)).at("definition")?;
        action
            .SetArguments(&BSTR::from(arguments.as_str()))
            .at("definition")?;
        let empty = VARIANT::default();
        let task = folder
            .RegisterTaskDefinition(
                &BSTR::from(name.as_str()),
                &definition,
                TASK_CREATE_OR_UPDATE.0,
                &empty,
                &empty,
                TASK_LOGON_INTERACTIVE_TOKEN,
                &empty,
            )
            .at("register")?;
        let running = task.Run(&empty).at("run")?;
        for _ in 0..100 {
            let pid = running.EnginePID().at("run")?;
            if pid != 0 {
                return Ok(pid);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            running.Refresh().at("run")?;
        }
        folder
            .DeleteTask(&BSTR::from(name.as_str()), 0)
            .at("cleanup")?;
        Err(Failure {
            stage: "run",
            code: E_FAIL,
        })
    })
}

pub(super) fn finish_update_helper(root: &Path, task_name: &str) -> Result<()> {
    if !valid_helper_task(root, task_name) {
        return Err("update-helper-task-invalid".into());
    }
    native::on_com("update-helper-task-cleanup-failed", || {
        let (_service, folder) = connect()?;
        match get_task(&folder, task_name)? {
            Some(task) => delete_helper(&folder, task_name, &task),
            None => Ok(()),
        }
    })
}

// A helper deletes its own task as it exits, so a task that disappears while
// another process removes it has already been removed.
fn unless_vanished(
    folder: &ITaskFolder,
    name: &str,
    remove: impl FnOnce() -> NativeResult<()>,
) -> NativeResult<()> {
    remove().or_else(|failure| match get_task(folder, name) {
        Ok(None) => Ok(()),
        _ => Err(failure),
    })
}

fn delete_helper(folder: &ITaskFolder, name: &str, task: &IRegisteredTask) -> NativeResult<()> {
    unless_vanished(folder, name, || {
        if !helper_owned(task)? {
            return Err(Failure {
                stage: "ownership",
                code: E_ACCESSDENIED,
            });
        }
        unsafe { folder.DeleteTask(&BSTR::from(name), 0) }.at("delete")
    })
}

fn stale_helper_date(date: &str, now: OffsetDateTime) -> bool {
    OffsetDateTime::parse(date, &Rfc3339)
        .is_ok_and(|date| now - date >= time::Duration::minutes(15))
}

fn remove_helper(
    folder: &ITaskFolder,
    name: &str,
    task: &IRegisteredTask,
    stale_only: bool,
) -> NativeResult<()> {
    unless_vanished(folder, name, || unsafe {
        if !helper_owned(task)? {
            return Ok(());
        }
        let instances = task.GetInstances(0).at("instances")?;
        let count = instances.Count().at("instances")?;
        if stale_only {
            if task.State().at("state")? == TASK_STATE_RUNNING || count != 0 {
                return Ok(());
            }
            let info = task
                .Definition()
                .at("read-definition")?
                .RegistrationInfo()
                .at("read-definition")?;
            let date = native::read_string("read-definition", |value| info.Date(value))?;
            if !stale_helper_date(&date, OffsetDateTime::now_utc()) {
                return Ok(());
            }
        } else {
            for index in 1..=count {
                instances
                    .get_Item(&VARIANT::from(index))
                    .at("instances")?
                    .Stop()
                    .at("stop")?;
            }
        }
        folder.DeleteTask(&BSTR::from(name), 0).at("delete")
    })
}

fn remove_helpers(root: &Path, stale_only: bool) -> Result<()> {
    native::on_com("update-helper-task-cleanup-failed", || unsafe {
        let (_service, folder) = connect()?;
        let tasks = folder.GetTasks(TASK_ENUM_HIDDEN.0).at("enumerate")?;
        let mut names = Vec::new();
        for index in 1..=tasks.Count().at("enumerate")? {
            let task = tasks.get_Item(&VARIANT::from(index)).at("enumerate")?;
            let name = task.Name().at("enumerate")?.to_string();
            if valid_helper_task(root, &name) {
                names.push(name);
            }
        }
        for name in names {
            if let Some(task) = get_task(&folder, &name)? {
                remove_helper(&folder, &name, &task, stale_only)?;
            }
        }
        Ok(())
    })
}

pub(super) fn sweep_stale_update_helpers(root: &Path) -> Result<()> {
    remove_helpers(root, true)
}
pub(super) fn cleanup_update_helpers(root: &Path) -> Result<()> {
    remove_helpers(root, false)
}

fn task_xml(root: &Path, executable: &Path) -> Result<String> {
    let root = root.to_str().ok_or("invalid-data-path")?;
    if root.contains('"') || root.chars().any(char::is_control) {
        return Err("invalid-data-path".into());
    }
    let program = xml(executable.to_str().ok_or("invalid-executable-path")?);
    let arguments = xml(&format!(
        "serve --data-dir \"{}\"",
        root.trim_end_matches(['\\', '/'])
    ));
    Ok(format!(
        r#"<?xml version="1.0"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
<RegistrationInfo><Description>RisuNest user sync server</Description></RegistrationInfo>
<Triggers><LogonTrigger><Enabled>true</Enabled><UserId>__CURRENT_SID__</UserId></LogonTrigger></Triggers>
<Principals><Principal id="User"><UserId>__CURRENT_SID__</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
<Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><AllowHardTerminate>true</AllowHardTerminate><StartWhenAvailable>true</StartWhenAvailable><RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable><IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings><AllowStartOnDemand>true</AllowStartOnDemand><Enabled>true</Enabled><Hidden>false</Hidden><ExecutionTimeLimit>PT0S</ExecutionTimeLimit><RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure></Settings>
<Actions Context="User"><Exec><Command>{program}</Command><Arguments>{arguments}</Arguments></Exec></Actions></Task>"#
    ))
}

fn update_task_xml(root: &Path, program: &Path, server: &Path) -> Result<String> {
    let root = root.to_str().ok_or("invalid-data-path")?;
    let server = server.to_str().ok_or("invalid-executable-path")?;
    if [root, server]
        .iter()
        .any(|value| value.contains('"') || value.chars().any(char::is_control))
    {
        return Err("invalid-update-task-path".into());
    }
    let program = xml(program.to_str().ok_or("invalid-executable-path")?);
    let arguments = xml(&format!(
        "--data-dir \"{}\" --server \"{}\" update scheduled",
        root.trim_end_matches(['\\', '/']),
        server
    ));
    Ok(format!(
        r#"<?xml version="1.0"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
<RegistrationInfo><Description>RisuNest user sync updater</Description></RegistrationInfo>
<Triggers><LogonTrigger><Enabled>true</Enabled><Delay>PT1M</Delay><UserId>__CURRENT_SID__</UserId></LogonTrigger><CalendarTrigger><StartBoundary>2026-01-01T00:00:00</StartBoundary><Enabled>true</Enabled><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay><Repetition><Interval>PT1H</Interval><Duration>P1D</Duration><StopAtDurationEnd>false</StopAtDurationEnd></Repetition></CalendarTrigger></Triggers>
<Principals><Principal id="User"><UserId>__CURRENT_SID__</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
<Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><StartWhenAvailable>true</StartWhenAvailable><RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable><AllowStartOnDemand>true</AllowStartOnDemand><Enabled>true</Enabled><Hidden>false</Hidden><ExecutionTimeLimit>PT30M</ExecutionTimeLimit></Settings>
<Actions Context="User"><Exec><Command>{program}</Command><Arguments>{arguments}</Arguments></Exec></Actions></Task>"#
    ))
}

fn action_matches(
    definition: &ITaskDefinition,
    program: &Path,
    arguments: &str,
) -> NativeResult<bool> {
    unsafe {
        let action = definition
            .Actions()
            .at("read-definition")?
            .get_Item(1)
            .at("read-definition")?;
        let mut kind = TASK_ACTION_TYPE::default();
        action.Type(&mut kind).at("read-definition")?;
        if kind != TASK_ACTION_EXEC {
            return Ok(false);
        }
        let action: IExecAction = action.cast().at("read-definition")?;
        let path = native::read_string("read-definition", |value| action.Path(value))?;
        let actual = native::read_string("read-definition", |value| action.Arguments(value))?;
        Ok(native::equal(
            &native::full_path(&path)?,
            &native::full_path(&program.to_string_lossy())?,
            true,
        )? && native::equal(&actual, arguments, false)?)
    }
}

fn task_operation(
    name: &str,
    description: &str,
    program: &Path,
    arguments: &str,
    xml: &str,
    action: &str,
) -> NativeResult<StartupStatus> {
    unsafe {
        let (_service, folder) = connect()?;
        let mut task = get_task(&folder, name)?;
        let sid = native::current_sid()?;
        let mut matches = true;
        if let Some(task) = &task {
            let definition = task.Definition().at("read-definition")?;
            let principal = definition.Principal().at("ownership")?;
            let user = native::read_string("ownership", |value| principal.UserId(value))?;
            let info = definition.RegistrationInfo().at("ownership")?;
            let actual_description =
                native::read_string("ownership", |value| info.Description(value))?;
            let mut count = 0;
            definition
                .Actions()
                .at("ownership")?
                .Count(&mut count)
                .at("ownership")?;
            if !native::equal(&native::account_sid(&user)?, &sid, true)?
                || count != 1
                || !native::equal(&actual_description, description, false)?
            {
                return Err(Failure {
                    stage: "ownership",
                    code: E_ACCESSDENIED,
                });
            }
            matches = action_matches(&definition, program, arguments).unwrap_or(false);
        }
        let empty = VARIANT::default();
        if matches!(action, "manual" | "install") {
            task = Some(
                folder
                    .RegisterTask(
                        &BSTR::from(name),
                        &BSTR::from(xml.replace("__CURRENT_SID__", &sid).as_str()),
                        TASK_CREATE_OR_UPDATE.0,
                        &VARIANT::from(sid.as_str()),
                        &empty,
                        TASK_LOGON_INTERACTIVE_TOKEN,
                        &empty,
                    )
                    .at("register")?,
            );
            matches = true;
        }
        match action {
            "manual" | "start" => {
                let task = task.as_ref().ok_or(Failure {
                    stage: "run",
                    code: HRESULT::from_win32(2),
                })?;
                if !matches {
                    return Err(Failure {
                        stage: "action",
                        code: E_ACCESSDENIED,
                    });
                }
                task.Run(&empty).at("run")?;
            }
            "remove" => {
                if task.is_some() {
                    folder.DeleteTask(&BSTR::from(name), 0).at("delete")?;
                }
                task = None;
                matches = true;
            }
            _ => (),
        }
        let mut registered = false;
        let mut enabled = false;
        if let Some(task) = task {
            let triggers = task
                .Definition()
                .at("read-definition")?
                .Triggers()
                .at("read-definition")?;
            let mut count = 0;
            triggers.Count(&mut count).at("read-definition")?;
            for index in 1..=count {
                let trigger = triggers.get_Item(index).at("read-definition")?;
                let mut kind = TASK_TRIGGER_TYPE2::default();
                let mut active = VARIANT_BOOL::default();
                trigger.Type(&mut kind).at("read-definition")?;
                trigger.Enabled(&mut active).at("read-definition")?;
                registered |= kind == TASK_TRIGGER_LOGON && active.as_bool();
            }
            enabled = registered && task.Enabled().at("read-definition")?.as_bool();
        }
        Ok(StartupStatus {
            registered,
            enabled,
            action_matches: matches,
        })
    }
}

pub(super) fn startup(root: &Path, executable: &Path, action: &str) -> Result<StartupStatus> {
    let definition = task_xml(root, executable)?;
    let definition = if action == "manual" {
        definition.replace("<Triggers><LogonTrigger><Enabled>true</Enabled><UserId>__CURRENT_SID__</UserId></LogonTrigger></Triggers>", "<Triggers/>")
    } else {
        definition
    };
    let arguments = format!(
        "serve --data-dir \"{}\"",
        root.to_string_lossy().trim_end_matches(['\\', '/'])
    );
    native::on_com("task-scheduler-operation-failed", || {
        task_operation(
            &instance_name(root),
            "RisuNest user sync server",
            executable,
            &arguments,
            &definition,
            action,
        )
    })
}

pub(super) fn update_schedule(
    root: &Path,
    manager: &Path,
    server: &Path,
    policy: UpdatePolicy,
    action: &str,
) -> Result<UpdateScheduleStatus> {
    let install = action == "install" && policy != UpdatePolicy::Off;
    let effective = if action == "remove" || (action == "install" && !install) {
        "remove"
    } else {
        action
    };
    let program = background_manager_executable(manager);
    if effective == "install" && !program.is_file() {
        return Err("manager-executable-missing".into());
    }
    let definition = update_task_xml(root, &program, server)?;
    let arguments = format!(
        "--data-dir \"{}\" --server \"{}\" update scheduled",
        root.to_string_lossy().trim_end_matches(['\\', '/']),
        server.to_string_lossy()
    );
    let status = native::on_com("update-task-scheduler-operation-failed", || {
        task_operation(
            &format!("{}-update", instance_name(root)),
            "RisuNest user sync updater",
            &program,
            &arguments,
            &definition,
            effective,
        )
    })?;
    Ok(UpdateScheduleStatus {
        registered: status.registered,
        enabled: status.enabled,
        action_matches: status.action_matches,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_helpers_accept_legacy_utc_dates_and_preserve_recent_or_invalid_dates() {
        let date = "2026-10-16T01:02:03.1234567Z";
        let created = OffsetDateTime::parse(date, &Rfc3339).unwrap();
        assert!(stale_helper_date(
            date,
            created + time::Duration::minutes(15)
        ));
        assert!(!stale_helper_date(
            date,
            created + time::Duration::minutes(15) - time::Duration::nanoseconds(1)
        ));
        assert!(!stale_helper_date(date, created - time::Duration::hours(1)));
        assert!(!stale_helper_date("invalid", created));
        assert!(!stale_helper_date("", created));
    }

    #[test]
    fn task_comparisons_preserve_windows_powershell_case_and_lexical_paths() {
        assert!(native::equal("Äbc 한글", "äBC 한글", false).unwrap());
        assert!(!native::equal("--data-dir a", "--data-dir b", false).unwrap());
        assert!(native::equal("", "", false).unwrap());
        assert!(!native::equal("", "a", false).unwrap());
        let left = native::full_path(r"C:\missing\parent\..\한글\SERVER.EXE").unwrap();
        assert!(native::equal(&left, r"c:\missing\한글\server.exe", true).unwrap());
        assert!(native::full_path("C:\\server.exe\0other").is_err());
    }

    #[test]
    fn native_com_calls_do_not_change_an_existing_runtime_apartment() {
        use ::windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
        std::thread::spawn(|| {
            unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
                .ok()
                .unwrap();
            let identity = native::on_com("test", native::current_sid).unwrap();
            assert!(identity.starts_with("S-1-"));
            let same_mode = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            unsafe {
                CoUninitialize();
            }
            same_mode.ok().unwrap();
            unsafe {
                CoUninitialize();
            }
        })
        .join()
        .unwrap();
    }

    #[test]
    fn scheduler_failures_keep_the_stage_and_hresult_without_arbitrary_output() {
        assert_eq!(
            native::diagnostic(
                "update-task-scheduler-operation-failed",
                Failure {
                    stage: "register",
                    code: E_ACCESSDENIED
                }
            ),
            "update-task-scheduler-operation-failed:register:80070005"
        );
    }

    #[test]
    fn helper_names_require_own_root_and_canonical_uuid() {
        let root = Path::new("synthetic-instance");
        let prefix = helper_task_prefix(root);
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        assert!(valid_helper_task(root, &format!("{prefix}{id}")));
        assert!(!valid_helper_task(
            Path::new("other-instance"),
            &format!("{prefix}{id}")
        ));
        for invalid in [
            id.to_uppercase(),
            id.replace("-4aaa-", "-7aaa-"),
            id.replace("-8aaa-", "-0aaa-"),
            format!("{id}\n"),
            "a".repeat(64),
        ] {
            assert!(!valid_helper_task(root, &format!("{prefix}{invalid}")));
        }
    }
    #[test]
    #[ignore = "requires the current-user Task Scheduler and creates only synthetic temporary tasks"]
    fn helper_tasks_that_vanish_during_removal_count_as_removed() {
        let root = tempfile::tempdir().unwrap();
        let task_name = || {
            format!(
                "{}{}",
                helper_task_prefix(root.path()),
                risunest_sync_server::management::discovery::request_id().unwrap()
            )
        };
        let [early, late, live, foreign] = [(); 4].map(|()| task_name());
        let outcome = native::on_com("synthetic-helper", || unsafe {
            let (service, folder) = connect()?;
            struct Cleanup(ITaskFolder, [String; 4]);
            impl Drop for Cleanup {
                fn drop(&mut self) {
                    for name in &self.1 {
                        let _ = unsafe { self.0.DeleteTask(&BSTR::from(name.as_str()), 0) };
                    }
                }
            }
            let _cleanup = Cleanup(
                folder.clone(),
                [early.clone(), late.clone(), live.clone(), foreign.clone()],
            );
            let register = |name: &str, description: &str| -> NativeResult<IRegisteredTask> {
                let definition = service.NewTask(0).at("definition")?;
                definition
                    .RegistrationInfo()
                    .at("definition")?
                    .SetDescription(&BSTR::from(description))
                    .at("definition")?;
                definition
                    .Principal()
                    .at("definition")?
                    .SetLogonType(TASK_LOGON_INTERACTIVE_TOKEN)
                    .at("definition")?;
                let action: IExecAction = definition
                    .Actions()
                    .at("definition")?
                    .Create(TASK_ACTION_EXEC)
                    .at("definition")?
                    .cast()
                    .at("definition")?;
                action.SetPath(&BSTR::from("cmd.exe")).at("definition")?;
                let empty = VARIANT::default();
                folder
                    .RegisterTaskDefinition(
                        &BSTR::from(name),
                        &definition,
                        TASK_CREATE.0,
                        &empty,
                        &empty,
                        TASK_LOGON_INTERACTIVE_TOKEN,
                        &empty,
                    )
                    .at("register")
            };
            let report = |result: NativeResult<()>| {
                result.map_err(|failure| native::diagnostic("cleanup", failure))
            };
            for name in [&early, &late, &live] {
                register(name, HELPER_TASK_DESCRIPTION)?;
            }
            // The helper deletes its own task after another process looked it
            // up: before the ownership check reads the stale handle, or after
            // it, when only the deletion by name fails. A handle that still
            // answers stands in for the second case.
            let stale = get_task(&folder, &early)?.expect("registered helper task");
            let answering = get_task(&folder, &live)?.expect("registered helper task");
            for name in [&early, &late] {
                folder
                    .DeleteTask(&BSTR::from(name.as_str()), 0)
                    .at("delete")?;
            }
            let mut removed = Vec::new();
            for (name, task) in [(&early, &stale), (&late, &answering)] {
                removed.push(report(remove_helper(&folder, name, task, false)));
                removed.push(report(delete_helper(&folder, name, task)));
            }
            let foreign_task = register(&foreign, "RisuNest foreign helper")?;
            let refused = report(delete_helper(&folder, &foreign, &foreign_task));
            let kept = get_task(&folder, &foreign)?.is_some();
            Ok((removed, refused, kept))
        });
        let (removed, refused, kept) = outcome.unwrap();
        assert_eq!(removed, vec![Ok(()); 4]);
        assert_eq!(refused, Err("cleanup:ownership:80070005".to_owned()));
        assert!(kept);
    }
    #[test]
    fn helper_arguments_preserve_spaces_quotes_and_trailing_slashes() {
        assert_eq!(quote_argument("plain"), "plain");
        assert_eq!(quote_argument("a b"), "\"a b\"");
        assert_eq!(quote_argument("a\\\"b"), "\"a\\\\\\\"b\"");
        assert_eq!(quote_argument("C:\\with space\\"), "\"C:\\with space\\\\\"");
    }
    #[test]
    fn task_uses_user_logon_and_preserves_quoted_paths() {
        let xml = task_xml(
            Path::new("C:\\Users\\Test & User\\sync"),
            Path::new("C:\\Apps\\Sync Manager\\server.exe"),
        )
        .unwrap();
        for value in [
            "InteractiveToken",
            "LeastPrivilege",
            "<ExecutionTimeLimit>PT0S",
            "<Count>3",
            "IgnoreNew",
            "Test &amp; User",
            "&quot;",
        ] {
            assert!(xml.contains(value));
        }
        assert!(!xml.contains("HighestAvailable"));
        assert!(!xml.contains("<Password>"));
        // RegisterTask receives a Unicode BSTR, not the UTF-8 file bytes.
        assert!(!xml.contains("encoding="));
    }

    #[test]
    fn updater_task_is_current_user_periodic_and_bounded() {
        let xml = update_task_xml(
            Path::new("C:\\Users\\Test & User\\sync"),
            &background_manager_executable(Path::new(
                "C:\\Apps\\RisuNest Sync\\risunest-sync-manager.exe",
            )),
            Path::new("C:\\Apps\\RisuNest Sync\\risunest-sync-server.exe"),
        )
        .unwrap();
        for value in [
            "<Command>C:\\Apps\\RisuNest Sync\\risunest-sync-manager-background.exe</Command>",
            "InteractiveToken",
            "LeastPrivilege",
            "<Interval>PT1H</Interval>",
            "<ExecutionTimeLimit>PT30M</ExecutionTimeLimit>",
            "<RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>",
            "update scheduled",
            "IgnoreNew",
        ] {
            assert!(xml.contains(value));
        }
        assert!(!xml.contains("HighestAvailable"));
        assert!(!xml.contains("<Password>"));
    }
}
