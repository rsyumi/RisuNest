use super::*;
use std::io::Write;

const HELPER_TASK_DESCRIPTION: &str = "RisuNest update helper";

fn helper_task_prefix(root: &Path) -> String {
    format!("{}-update-helper-", instance_name(root))
}

fn valid_helper_task(root: &Path, task_name: &str) -> bool {
    let prefix = helper_task_prefix(root);
    task_name.starts_with(&prefix)
        && task_name.len() == prefix.len() + 64
        && task_name[prefix.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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

pub(super) fn spawn_update_helper(root: &Path, command: &mut Command) -> Result<u32> {
    const SPAWN: &str = r#"$ErrorActionPreference='Stop';$s=New-Object -ComObject 'Schedule.Service';$s.Connect();$f=$s.GetFolder('\');$d=$s.NewTask(0);$d.RegistrationInfo.Description='RisuNest update helper';$d.Principal.LogonType=3;$d.Principal.RunLevel=0;$d.Settings.Enabled=$true;$d.Settings.StartWhenAvailable=$true;$d.Settings.ExecutionTimeLimit='PT10M';$a=$d.Actions.Create(0);$a.Path=$env:RISUNEST_HELPER_PROGRAM;$a.Arguments=$env:RISUNEST_HELPER_ARGUMENTS;$r=$f.RegisterTaskDefinition($env:RISUNEST_HELPER_TASK,$d,6,$null,$null,3,$null);$run=$r.Run($null);for($i=0;$i-lt 100-and $run.EnginePID-eq 0;$i++){Start-Sleep -Milliseconds 20};$pidValue=$run.EnginePID;if($pidValue-eq 0){$f.DeleteTask($env:RISUNEST_HELPER_TASK,0);throw 'helper did not start'};[Console]::Out.Write($pidValue)"#;
    let task_name = format!(
        "{}-update-helper-{}",
        instance_name(root),
        risunest_sync_server::management::discovery::request_id()
            .map_err(|_| "update-helper-start-failed".to_owned())?
    );
    command.args(["--scheduled-task", &task_name]);
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
    let output = process("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            SPAWN,
        ])
        .env("RISUNEST_HELPER_TASK", task_name)
        .env("RISUNEST_HELPER_PROGRAM", program)
        .env("RISUNEST_HELPER_ARGUMENTS", arguments)
        .stdin(Stdio::null())
        .output()
        .map_err(|_| "update-helper-start-failed".to_owned())?;
    if !output.status.success() {
        return Err("update-helper-start-failed".into());
    }
    String::from_utf8(output.stdout)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .ok_or("update-helper-start-failed".into())
}

pub(super) fn finish_update_helper(root: &Path, task_name: &str) -> Result<()> {
    const REMOVE: &str = r#"$ErrorActionPreference='Stop';$s=New-Object -ComObject 'Schedule.Service';$s.Connect();$f=$s.GetFolder('\');try{$t=$f.GetTask($env:RISUNEST_HELPER_TASK)}catch{$r=$_.Exception;while($r.InnerException){$r=$r.InnerException};if($r.HResult -eq -2147024894 -or $r.HResult -eq -2147024893){exit 0};throw};if($t.Definition.RegistrationInfo.Description -ne $env:RISUNEST_HELPER_DESCRIPTION){throw 'unexpected helper task owner'};$f.DeleteTask($env:RISUNEST_HELPER_TASK,0)"#;
    if !valid_helper_task(root, task_name) {
        return Err("update-helper-task-invalid".into());
    }
    let status = process("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            REMOVE,
        ])
        .env("RISUNEST_HELPER_TASK", task_name)
        .env("RISUNEST_HELPER_DESCRIPTION", HELPER_TASK_DESCRIPTION)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| "update-helper-task-cleanup-failed".to_owned())?;
    if status.success() {
        Ok(())
    } else {
        Err("update-helper-task-cleanup-failed".into())
    }
}

pub(super) fn cleanup_update_helpers(root: &Path) -> Result<()> {
    const CLEAN: &str = r#"$ErrorActionPreference='Stop';$s=New-Object -ComObject 'Schedule.Service';$s.Connect();$f=$s.GetFolder('\');foreach($t in @($f.GetTasks(1))){$n=$t.Name;if(!$n.StartsWith($env:RISUNEST_HELPER_PREFIX,[StringComparison]::Ordinal)){continue};$x=$n.Substring($env:RISUNEST_HELPER_PREFIX.Length);if($x.Length -ne 64 -or $x -cnotmatch '^[0-9a-f]{64}$'){continue};if($t.Definition.RegistrationInfo.Description -ne $env:RISUNEST_HELPER_DESCRIPTION){continue};foreach($i in @($t.GetInstances(0))){$i.Stop()};$f.DeleteTask($n,0)}"#;
    let status = process("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            CLEAN,
        ])
        .env("RISUNEST_HELPER_PREFIX", helper_task_prefix(root))
        .env("RISUNEST_HELPER_DESCRIPTION", HELPER_TASK_DESCRIPTION)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| "update-helper-task-cleanup-failed".to_owned())?;
    if status.success() {
        Ok(())
    } else {
        Err("update-helper-task-cleanup-failed".into())
    }
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

fn update_task_xml(root: &Path, manager: &Path, server: &Path) -> Result<String> {
    let root = root.to_str().ok_or("invalid-data-path")?;
    let server = server.to_str().ok_or("invalid-executable-path")?;
    if [root, server]
        .iter()
        .any(|value| value.contains('"') || value.chars().any(char::is_control))
    {
        return Err("invalid-update-task-path".into());
    }
    let program = xml(manager.to_str().ok_or("invalid-executable-path")?);
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
<Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><StartWhenAvailable>true</StartWhenAvailable><RunOnlyIfNetworkAvailable>true</RunOnlyIfNetworkAvailable><AllowStartOnDemand>true</AllowStartOnDemand><Enabled>true</Enabled><Hidden>false</Hidden><ExecutionTimeLimit>PT30M</ExecutionTimeLimit></Settings>
<Actions Context="User"><Exec><Command>{program}</Command><Arguments>{arguments}</Arguments></Exec></Actions></Task>"#
    ))
}

const SCRIPT: &str = r#"
$ErrorActionPreference='Stop'
$stage='connect'
try {
 $service=New-Object -ComObject 'Schedule.Service'; $service.Connect(); $folder=$service.GetFolder('\')
 $task=$null
 try { $task=$folder.GetTask($env:RISUNEST_TASK_NAME) } catch {
  $reason=$_.Exception; while($reason.InnerException){$reason=$reason.InnerException}
  if($reason.HResult -ne -2147024894 -and $reason.HResult -ne -2147024893){throw}
 }
 $sid=[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
 $actionMatches=$true
 if($null -ne $task) {
  $stage='ownership'
  $definition=$task.Definition
  $principal=$definition.Principal.UserId
  if($principal -notlike 'S-1-*'){$principal=([System.Security.Principal.NTAccount]$principal).Translate([System.Security.Principal.SecurityIdentifier]).Value}
  if($principal -ne $sid -or $definition.Actions.Count -ne 1 -or $definition.RegistrationInfo.Description -ne $env:RISUNEST_TASK_DESCRIPTION){throw 'unexpected-task-owner'}
  $action=$definition.Actions.Item(1)
  $actionMatches=$false
  if($action.Type -eq 0) {
   try { $actionMatches=[StringComparer]::OrdinalIgnoreCase.Equals([IO.Path]::GetFullPath($action.Path),[IO.Path]::GetFullPath($env:RISUNEST_TASK_PROGRAM)) -and $action.Arguments -eq $env:RISUNEST_TASK_ARGUMENTS } catch { $actionMatches=$false }
  }
 }
 switch($env:RISUNEST_TASK_ACTION) {
  'install' {
   $stage='read-definition'
   $sid=[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
   $definition=[IO.File]::ReadAllText($env:RISUNEST_TASK_XML).Replace('__CURRENT_SID__',$sid)
   $stage='register'
   $task=$folder.RegisterTask($env:RISUNEST_TASK_NAME,$definition,6,$sid,$null,3,$null)
   $actionMatches=$true
  }
  'remove' { if($null -ne $task){$folder.DeleteTask($env:RISUNEST_TASK_NAME,0)}; $task=$null; $actionMatches=$true }
  'start' { if($null -eq $task){throw 'missing-task'}; if(!$actionMatches){throw 'unexpected-task-action'}; $null=$task.Run($null) }
 }
 @{registered=($null -ne $task);enabled=($null -ne $task -and $task.Enabled);actionMatches=$actionMatches} | ConvertTo-Json -Compress
} catch {
 $reason=$_.Exception; while($reason.InnerException){$reason=$reason.InnerException}
 [Console]::Error.WriteLine($stage+':'+$reason.HResult.ToString('X8'))
 exit 1
}
"#;

pub(super) fn startup(root: &Path, executable: &Path, action: &str) -> Result<StartupStatus> {
    let mut file = tempfile::NamedTempFile::new().map_err(|_| "startup-file-unavailable")?;
    file.write_all(task_xml(root, executable)?.as_bytes())
        .map_err(|_| "startup-file-unavailable")?;
    // Close the delete-on-close handle before .NET opens the definition. Its
    // default FileShare.Read cannot coexist with that Windows DELETE access.
    let path = file.into_temp_path();
    let output = process("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            SCRIPT,
        ])
        .env("RISUNEST_TASK_NAME", instance_name(root))
        .env("RISUNEST_TASK_ACTION", action)
        .env("RISUNEST_TASK_PROGRAM", executable)
        .env("RISUNEST_TASK_DESCRIPTION", "RisuNest user sync server")
        .env(
            "RISUNEST_TASK_ARGUMENTS",
            format!(
                "serve --data-dir \"{}\"",
                root.to_string_lossy().trim_end_matches(['\\', '/'])
            ),
        )
        .env("RISUNEST_TASK_XML", &path)
        .stdin(Stdio::null())
        .output()
        .map_err(|_| "task-scheduler-unavailable")?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim();
        if detail.len() < 80
            && detail
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c.is_ascii_lowercase() || c == '-' || c == ':')
        {
            return Err(format!("task-scheduler-operation-failed:{detail}"));
        }
        return Err("task-scheduler-operation-failed".into());
    }
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|_| "task-scheduler-response-invalid")?;
    Ok(StartupStatus {
        registered: value["registered"]
            .as_bool()
            .ok_or("task-scheduler-response-invalid")?,
        enabled: value["enabled"]
            .as_bool()
            .ok_or("task-scheduler-response-invalid")?,
        action_matches: value["actionMatches"]
            .as_bool()
            .ok_or("task-scheduler-response-invalid")?,
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
    let mut file = tempfile::NamedTempFile::new().map_err(|_| "update-task-file-unavailable")?;
    file.write_all(update_task_xml(root, manager, server)?.as_bytes())
        .map_err(|_| "update-task-file-unavailable")?;
    let path = file.into_temp_path();
    let arguments = format!(
        "--data-dir \"{}\" --server \"{}\" update scheduled",
        root.to_string_lossy().trim_end_matches(['\\', '/']),
        server.to_string_lossy()
    );
    let output = process("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            SCRIPT,
        ])
        .env(
            "RISUNEST_TASK_NAME",
            format!("{}-update", instance_name(root)),
        )
        .env("RISUNEST_TASK_ACTION", effective)
        .env("RISUNEST_TASK_PROGRAM", manager)
        .env("RISUNEST_TASK_ARGUMENTS", arguments)
        .env("RISUNEST_TASK_DESCRIPTION", "RisuNest user sync updater")
        .env("RISUNEST_TASK_XML", &path)
        .stdin(Stdio::null())
        .output()
        .map_err(|_| "task-scheduler-unavailable")?;
    if !output.status.success() {
        return Err("update-task-scheduler-operation-failed".into());
    }
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|_| "task-scheduler-response-invalid")?;
    Ok(UpdateScheduleStatus {
        registered: value["registered"]
            .as_bool()
            .ok_or("task-scheduler-response-invalid")?,
        enabled: value["enabled"]
            .as_bool()
            .ok_or("task-scheduler-response-invalid")?,
        action_matches: value["actionMatches"]
            .as_bool()
            .ok_or("task-scheduler-response-invalid")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
            Path::new("C:\\Apps\\RisuNest Sync\\risunest-sync-manager.exe"),
            Path::new("C:\\Apps\\RisuNest Sync\\risunest-sync-server.exe"),
        )
        .unwrap();
        for value in [
            "InteractiveToken",
            "LeastPrivilege",
            "<Interval>PT1H</Interval>",
            "<ExecutionTimeLimit>PT30M</ExecutionTimeLimit>",
            "update scheduled",
            "IgnoreNew",
        ] {
            assert!(xml.contains(value));
        }
        assert!(!xml.contains("HighestAvailable"));
        assert!(!xml.contains("<Password>"));
    }
}
