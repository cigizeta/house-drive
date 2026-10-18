//! The Task Scheduler task that runs `sync` by itself: every `sync_minutes` and at logon, as the
//! user (least privilege, never SYSTEM), never two at once, on battery too.
//!
//! The task starts `house-drive-sync.exe`, a program with no console, so no window opens every few
//! minutes; it runs `house-drive sync` hidden and keeps its output in `sync.log`.

use std::io;
use std::path::Path;
use std::process::Command;

pub const TASK_NAME: &str = "House Drive";

fn escape(text: &str) -> String {
    return text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
}

/// The task's definition, in Task Scheduler's XML.
pub fn task_xml(user: &str, minutes: u32, program: &Path) -> String {
    let user = escape(user);
    let program = escape(&program.display().to_string());
    return format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>House Drive: sync this machine with the store</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
    <TimeTrigger>
      <Repetition>
        <Interval>PT{minutes}M</Interval>
        <StopAtDurationEnd>false</StopAtDurationEnd>
      </Repetition>
      <StartBoundary>2000-01-01T00:00:00</StartBoundary>
      <Enabled>true</Enabled>
    </TimeTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <StartWhenAvailable>true</StartWhenAvailable>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Enabled>true</Enabled>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{program}</Command>
    </Exec>
  </Actions>
</Task>
"#
    );
}

/// Registers (or replaces) the task from its definition, written as UTF-16 with a byte order
/// mark, which is what schtasks reads reliably.
pub fn register(xml: &str, xml_path: &Path) -> Result<(), String> {
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
    std::fs::write(xml_path, bytes).map_err(|error| format!("{}: {error}", xml_path.display()))?;
    let xml_arg = xml_path.display().to_string();
    return run_schtasks(&["/Create", "/TN", TASK_NAME, "/XML", &xml_arg, "/F"]);
}

pub fn unregister() -> Result<(), String> {
    return run_schtasks(&["/Delete", "/TN", TASK_NAME, "/F"]);
}

fn run_schtasks(args: &[&str]) -> Result<(), String> {
    let output = Command::new("schtasks.exe")
        .args(args)
        .output()
        .map_err(|error: io::Error| format!("schtasks.exe: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
    return Err(format!("schtasks.exe failed: {message}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_definition_holds_the_interval_and_escapes_values() {
        let xml = task_xml(r"MACHINE-A\user", 10, Path::new(r"C:\a&b\house-drive-sync.exe"));
        assert!(xml.contains("<Interval>PT10M</Interval>"));
        assert!(xml.contains(r"<UserId>MACHINE-A\user</UserId>"));
        assert!(xml.contains(r"<Command>C:\a&amp;b\house-drive-sync.exe</Command>"));
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
    }
}
