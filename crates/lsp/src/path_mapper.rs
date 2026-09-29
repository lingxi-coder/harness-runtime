use lingxi_core::host::LspError;
use lsp_types::Url;
use std::path::{Path, PathBuf};

/// One document's dual identity: host path for local file I/O and server path
/// for guest-visible LSP URIs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspDocumentPath {
    pub host_path: PathBuf,
    pub server_path: PathBuf,
    pub uri: Url,
}

pub trait LspPathMapper: Send + Sync {
    fn map_host_path(&self, path: &Path, workspace_cwd: &Path)
        -> Result<LspDocumentPath, LspError>;
    fn workspace_root_for(&self, workspace_cwd: &Path) -> Result<PathBuf, LspError>;
    fn uri_for_host_path(&self, path: &Path) -> Result<Url, LspError>;
    fn host_path_for_uri(&self, uri: &Url) -> Result<Option<PathBuf>, LspError> {
        match uri.to_file_path() {
            Ok(path) => Ok(Some(path)),
            Err(()) => Ok(None),
        }
    }
    fn host_uri_for_server_uri(&self, uri: &Url) -> Result<Option<Url>, LspError> {
        let Some(path) = self.host_path_for_uri(uri)? else {
            return Ok(None);
        };
        Url::from_file_path(&path).map(Some).map_err(|()| {
            LspError::Transport(format!(
                "cannot convert path to file URI: {}",
                path.display()
            ))
        })
    }
}

#[derive(Debug)]
pub struct DesktopLspPathMapper;

impl LspPathMapper for DesktopLspPathMapper {
    fn map_host_path(
        &self,
        path: &Path,
        _workspace_cwd: &Path,
    ) -> Result<LspDocumentPath, LspError> {
        let host_path = path.to_path_buf();
        let uri = self.uri_for_host_path(&host_path)?;
        Ok(LspDocumentPath {
            host_path,
            server_path: path.to_path_buf(),
            uri,
        })
    }

    fn workspace_root_for(&self, workspace_cwd: &Path) -> Result<PathBuf, LspError> {
        Ok(workspace_cwd.to_path_buf())
    }

    fn uri_for_host_path(&self, path: &Path) -> Result<Url, LspError> {
        Url::from_file_path(path).map_err(|()| {
            LspError::Transport(format!(
                "cannot convert path to file URI: {}",
                path.display()
            ))
        })
    }
}
