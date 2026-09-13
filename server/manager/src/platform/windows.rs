use super::*;
use std::io::Write;

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
 if($null -ne $task) {
  $stage='ownership'
  $definition=$task.Definition
  $principal=$definition.Principal.UserId
  if($principal -notlike 'S-1-*'){$principal=([System.Security.Principal.NTAccount]$principal).Translate([System.Security.Principal.SecurityIdentifier]).Value}
  if($principal -ne $sid -or $definition.Actions.Count -ne 1 -or $definition.RegistrationInfo.Description -ne 'RisuNest user sync server'){throw 'unexpected-task-owner'}
  $action=$definition.Actions.Item(1)
  if($action.Type -ne 0 -or ![StringComparer]::OrdinalIgnoreCase.Equals([IO.Path]::GetFullPath($action.Path),[IO.Path]::GetFullPath($env:RISUNEST_TASK_PROGRAM)) -or $action.Arguments -ne $env:RISUNEST_TASK_ARGUMENTS){throw 'unexpected-task-action'}
 }
 switch($env:RISUNEST_TASK_ACTION) {
  'install' {
   $stage='read-definition'
   $sid=[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
   $definition=[IO.File]::ReadAllText($env:RISUNEST_TASK_XML).Replace('__CURRENT_SID__',$sid)
   $stage='register'
   $task=$folder.RegisterTask($env:RISUNEST_TASK_NAME,$definition,6,$sid,$null,3,$null)
  }
  'remove' { if($null -ne $task){$folder.DeleteTask($env:RISUNEST_TASK_NAME,0)}; $task=$null }
  'start' { if($null -eq $task){throw 'missing-task'}; $null=$task.Run($null) }
 }
 @{registered=($null -ne $task);enabled=($null -ne $task -and $task.Enabled)} | ConvertTo-Json -Compress
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
