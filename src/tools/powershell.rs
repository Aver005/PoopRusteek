use super::*;
use serde_json::{json, Value};
use tokio::process::Command;

pub struct PowerShellTool;

#[async_trait]
impl Tool for PowerShellTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "powershell".to_string(),
            description: "Execute a PowerShell command and return its output".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The PowerShell command to execute"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let command = match args["command"].as_str() {
            Some(cmd) => cmd,
            None => return ToolResult::error("Missing 'command' argument"),
        };

        let output = Command::new("powershell")
            .arg("-NoProfile")
            .arg("-Command")
            .arg(command)
            .output()
            .await;

        match output {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                if output.status.success() {
                    ToolResult::success(&stdout)
                } else {
                    let mut result = String::new();
                    if !stdout.is_empty() {
                        result.push_str(&stdout);
                    }
                    if !stderr.is_empty() {
                        if !result.is_empty() {
                            result.push('\n');
                        }
                        result.push_str(&stderr);
                    }
                    ToolResult::error(&result)
                }
            }
            Err(e) => ToolResult::error(&format!("Failed to execute command: {e}")),
        }
    }
}
