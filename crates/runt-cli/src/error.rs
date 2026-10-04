use std::fmt;

/// Exit code for runt's own failures (the `docker run` / `chroot` convention).
pub const EXIT_RUNT_ERROR: i32 = 125;

/// An error reported to the user (or agent). `code` is a stable machine-readable
/// identifier; `hint` suggests what to do next.
#[derive(Debug)]
pub struct CliError {
    pub code: &'static str,
    pub message: String,
    pub hint: Option<String>,
}

pub type Result<T> = std::result::Result<T, CliError>;

impl CliError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        CliError {
            code,
            message: message.into(),
            hint: None,
        }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn print(&self, json: bool) {
        if json {
            let v = serde_json::json!({
                "error": { "code": self.code, "message": self.message, "hint": self.hint }
            });
            eprintln!("{v}");
        } else {
            eprintln!("runt: {}", self.message);
            if let Some(h) = &self.hint {
                eprintln!("hint: {h}");
            }
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        CliError::new("io", e.to_string())
    }
}
