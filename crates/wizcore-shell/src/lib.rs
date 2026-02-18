#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    #[error("not implemented")]
    NotImplemented,
}

pub trait ShellActions {
    fn open_path(&self, full_path: &str) -> Result<(), ShellError>;
    fn reveal_path(&self, full_path: &str) -> Result<(), ShellError>;
}
