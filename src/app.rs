use anyhow::{Result, anyhow};
use crossterm::event::{Event, KeyCode};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

use crate::sftp::{FileInfo, SftpClient};
use crate::ssh_config::{SshConfig, SshHost};
use crate::ui::Ui;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pane {
    Local,
    Remote,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bookmark {
    pub name: String,
    pub local_path: PathBuf,
    pub remote_path: PathBuf,
    pub host: Option<String>,
    pub active_pane: Pane,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncDirection {
    LocalToRemote,
    RemoteToLocal,
}

#[derive(Debug, Clone)]
pub enum TransferDirection {
    Upload,
    Download,
}

#[derive(Debug, Clone)]
pub struct TransferItem {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub direction: TransferDirection,
}

pub struct App {
    pub ssh_config: SshConfig,
    pub sftp_client: Option<SftpClient>,
    pub current_host: Option<String>,
    pub available_hosts: Vec<SshHost>,

    pub active_pane: Pane,
    pub local_path: PathBuf,
    pub remote_path: PathBuf,
    pub local_files: Vec<FileInfo>,
    pub remote_files: Vec<FileInfo>,
    pub local_cursor: usize,
    pub remote_cursor: usize,
    pub local_selected: HashSet<usize>,
    pub remote_selected: HashSet<usize>,

    pub show_connection_dialog: bool,
    pub connection_cursor: usize,
    pub show_transfer_dialog: bool,
    pub transfer_queue: Vec<TransferItem>,

    pub bookmarks: Vec<Bookmark>,
    pub show_bookmark_dialog: bool,
    pub bookmark_cursor: usize,
    pub bookmark_editing: bool,
    pub bookmark_name: String,

    pub show_sync_dialog: bool,
    pub sync_direction: SyncDirection,
    pub sync_dry_run: bool,
    pub sync_completed: bool,
    pub sync_output: String,

    pub search_mode: bool,
    pub search_query: String,
    pub filtered_local_files: Vec<FileInfo>,
    pub filtered_remote_files: Vec<FileInfo>,

    pub should_quit: bool,
}

impl App {
    pub async fn new(initial_host: Option<String>) -> Result<Self> {
        let ssh_config = SshConfig::new()?;
        let available_hosts = ssh_config.get_all_hosts();

        let local_path = env::current_dir()?;
        let remote_path = PathBuf::from("/");

        let mut app = App {
            ssh_config,
            sftp_client: None,
            current_host: None,
            available_hosts,

            active_pane: Pane::Local,
            local_path,
            remote_path,
            local_files: Vec::new(),
            remote_files: Vec::new(),
            local_cursor: 0,
            remote_cursor: 0,
            local_selected: HashSet::new(),
            remote_selected: HashSet::new(),

            show_connection_dialog: false,
            connection_cursor: 0,
            show_transfer_dialog: false,
            transfer_queue: Vec::new(),

            bookmarks: Self::load_bookmarks(),
            show_bookmark_dialog: false,
            bookmark_cursor: 0,
            bookmark_editing: false,
            bookmark_name: String::new(),

            show_sync_dialog: false,
            sync_direction: SyncDirection::LocalToRemote,
            // A sync with --delete can remove files from the destination. Make
            // the first run a preview so it is safe to inspect the changes.
            sync_dry_run: true,
            sync_completed: false,
            sync_output: String::new(),

            search_mode: false,
            search_query: String::new(),
            filtered_local_files: Vec::new(),
            filtered_remote_files: Vec::new(),

            should_quit: false,
        };

        app.refresh_local_files()?;

        if let Some(host) = initial_host {
            app.connect_to_host(&host).await?;
        }

        Ok(app)
    }

    pub async fn run(&mut self) -> Result<()> {
        let mut ui = Ui::new()?;

        loop {
            if self.should_quit {
                break;
            }

            ui.draw(self)?;

            if let Some(event) = ui.handle_events()? {
                self.handle_event(event).await?;
            }
        }

        Ok(())
    }

    async fn handle_event(&mut self, event: Event) -> Result<()> {
        if let Event::Key(key) = event {
            if self.show_connection_dialog {
                return self.handle_connection_dialog_event(key.code).await;
            }

            if self.show_bookmark_dialog {
                return self.handle_bookmark_dialog_event(key.code).await;
            }

            if self.show_sync_dialog {
                return self.handle_sync_dialog_event(key.code).await;
            }

            if self.show_transfer_dialog {
                return self.handle_transfer_dialog_event(key.code).await;
            }

            if self.search_mode {
                return self.handle_search_event(key.code).await;
            }

            match key.code {
                KeyCode::Char('q') | KeyCode::Char('Q') => {
                    self.should_quit = true;
                }
                KeyCode::Tab => {
                    self.active_pane = match self.active_pane {
                        Pane::Local => Pane::Remote,
                        Pane::Remote => Pane::Local,
                    };
                }
                KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => {
                    self.move_cursor_up();
                }
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => {
                    self.move_cursor_down();
                }
                // Yazi/vim-style directory navigation.  These actions use the
                // active pane, so the same bindings work for local and remote
                // listings.
                KeyCode::Char('h') | KeyCode::Char('H') => {
                    self.go_to_parent_directory().await?;
                }
                KeyCode::Char('l') | KeyCode::Char('L') | KeyCode::Enter => {
                    self.change_directory().await?;
                }
                KeyCode::Char('g') => {
                    self.move_cursor_top();
                }
                KeyCode::Char('G') => {
                    self.move_cursor_bottom();
                }
                KeyCode::Char(' ') => {
                    self.toggle_selection();
                }
                KeyCode::Char('c') | KeyCode::Char('C') => {
                    self.show_connection_dialog = true;
                }
                KeyCode::Char('t') | KeyCode::Char('T') => {
                    self.prepare_transfer()?;
                }
                KeyCode::Char('b') => {
                    self.show_bookmark_dialog = true;
                    self.bookmark_editing = false;
                    self.bookmark_cursor = self
                        .bookmark_cursor
                        .min(self.bookmarks.len().saturating_sub(1));
                }
                KeyCode::Char('m') => {
                    self.show_bookmark_dialog = true;
                    self.bookmark_editing = true;
                    self.bookmark_name.clear();
                }
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    self.show_sync_dialog = true;
                    self.sync_completed = false;
                    self.sync_output.clear();
                    self.sync_dry_run = true;
                    self.sync_direction = match self.active_pane {
                        Pane::Local => SyncDirection::LocalToRemote,
                        Pane::Remote => SyncDirection::RemoteToLocal,
                    };
                }
                KeyCode::Char('/') => {
                    self.start_search();
                }
                _ => {}
            }
        }

        Ok(())
    }

    async fn handle_bookmark_dialog_event(&mut self, key: KeyCode) -> Result<()> {
        if self.bookmark_editing {
            match key {
                KeyCode::Esc => {
                    self.show_bookmark_dialog = false;
                    self.bookmark_editing = false;
                    self.bookmark_name.clear();
                }
                KeyCode::Backspace => {
                    self.bookmark_name.pop();
                }
                KeyCode::Enter => {
                    let name = self.bookmark_name.trim().to_string();
                    if !name.is_empty() {
                        self.save_bookmark(name)?;
                        self.show_bookmark_dialog = false;
                        self.bookmark_editing = false;
                        self.bookmark_name.clear();
                    }
                }
                KeyCode::Char(c) => self.bookmark_name.push(c),
                _ => {}
            }
            return Ok(());
        }

        match key {
            KeyCode::Esc => self.show_bookmark_dialog = false,
            KeyCode::Up | KeyCode::Char('k') => {
                if self.bookmark_cursor > 0 {
                    self.bookmark_cursor -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.bookmark_cursor < self.bookmarks.len().saturating_sub(1) {
                    self.bookmark_cursor += 1;
                }
            }
            KeyCode::Char('a') | KeyCode::Char('m') => {
                self.bookmark_editing = true;
                self.bookmark_name.clear();
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if !self.bookmarks.is_empty() {
                    self.bookmarks.remove(self.bookmark_cursor);
                    self.bookmark_cursor = self
                        .bookmark_cursor
                        .min(self.bookmarks.len().saturating_sub(1));
                    self.persist_bookmarks()?;
                }
            }
            KeyCode::Enter => {
                if let Some(bookmark) = self.bookmarks.get(self.bookmark_cursor).cloned() {
                    self.restore_bookmark(&bookmark).await?;
                    self.show_bookmark_dialog = false;
                }
            }
            _ => {}
        }

        Ok(())
    }

    async fn handle_sync_dialog_event(&mut self, key: KeyCode) -> Result<()> {
        if self.sync_completed {
            if matches!(key, KeyCode::Esc | KeyCode::Enter) {
                self.show_sync_dialog = false;
            }
            return Ok(());
        }

        match key {
            KeyCode::Esc => self.show_sync_dialog = false,
            KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                self.sync_direction = match self.sync_direction {
                    SyncDirection::LocalToRemote => SyncDirection::RemoteToLocal,
                    SyncDirection::RemoteToLocal => SyncDirection::LocalToRemote,
                };
            }
            KeyCode::Char('d') => self.sync_dry_run = !self.sync_dry_run,
            KeyCode::Enter => {
                match self.run_rsync().await {
                    Ok(output) => self.sync_output = output,
                    Err(error) => self.sync_output = format!("Sync failed: {error}"),
                }
                self.sync_completed = true;
                self.refresh_local_files()?;
                self.refresh_remote_files().await?;
            }
            _ => {}
        }

        Ok(())
    }

    async fn handle_connection_dialog_event(&mut self, key: KeyCode) -> Result<()> {
        match key {
            KeyCode::Esc => {
                self.show_connection_dialog = false;
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => {
                if self.connection_cursor > 0 {
                    self.connection_cursor -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => {
                if self.connection_cursor < self.available_hosts.len().saturating_sub(1) {
                    self.connection_cursor += 1;
                }
            }
            KeyCode::Char('g') => {
                self.connection_cursor = 0;
            }
            KeyCode::Char('G') => {
                self.connection_cursor = self.available_hosts.len().saturating_sub(1);
            }
            KeyCode::Enter => {
                if let Some(host) = self.available_hosts.get(self.connection_cursor).cloned() {
                    self.connect_to_host(&host.host).await?;
                    self.show_connection_dialog = false;
                }
            }
            _ => {}
        }

        Ok(())
    }

    async fn handle_transfer_dialog_event(&mut self, key: KeyCode) -> Result<()> {
        match key {
            KeyCode::Esc => {
                self.show_transfer_dialog = false;
                self.transfer_queue.clear();
            }
            KeyCode::Enter => {
                self.execute_transfers().await?;
                self.show_transfer_dialog = false;
            }
            _ => {}
        }

        Ok(())
    }

    async fn connect_to_host(&mut self, host_name: &str) -> Result<()> {
        let host_config = self
            .ssh_config
            .get_host(host_name)
            .unwrap_or_else(|| SshHost {
                host: host_name.to_string(),
                hostname: Some(host_name.to_string()),
                user: None,
                port: None,
                identity_file: None,
                proxy_jump: None,
            });

        let client = SftpClient::connect(&host_config)?;
        self.sftp_client = Some(client);
        self.current_host = Some(host_name.to_string());
        self.remote_path = PathBuf::from("/");
        self.refresh_remote_files().await?;

        Ok(())
    }

    fn bookmarks_path() -> Option<PathBuf> {
        dirs::config_dir().map(|path| path.join("sftui").join("bookmarks.json"))
    }

    fn load_bookmarks() -> Vec<Bookmark> {
        let Some(path) = Self::bookmarks_path() else {
            return Vec::new();
        };

        fs::read_to_string(path)
            .ok()
            .and_then(|contents| serde_json::from_str(&contents).ok())
            .unwrap_or_default()
    }

    fn persist_bookmarks(&self) -> Result<()> {
        let Some(path) = Self::bookmarks_path() else {
            return Ok(());
        };

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let contents = serde_json::to_string_pretty(&self.bookmarks)?;
        fs::write(path, contents)?;
        Ok(())
    }

    fn save_bookmark(&mut self, name: String) -> Result<()> {
        let bookmark = Bookmark {
            name: name.clone(),
            local_path: self.local_path.clone(),
            remote_path: self.remote_path.clone(),
            host: self.current_host.clone(),
            active_pane: self.active_pane,
        };

        if let Some(index) = self.bookmarks.iter().position(|item| item.name == name) {
            self.bookmarks[index] = bookmark;
            self.bookmark_cursor = index;
        } else {
            self.bookmarks.push(bookmark);
            self.bookmark_cursor = self.bookmarks.len().saturating_sub(1);
        }

        self.persist_bookmarks()
    }

    async fn restore_bookmark(&mut self, bookmark: &Bookmark) -> Result<()> {
        if !bookmark.local_path.is_dir() {
            return Err(anyhow!(
                "local bookmark directory does not exist: {}",
                bookmark.local_path.display()
            ));
        }

        let needs_connection = bookmark.host != self.current_host
            || (bookmark.host.is_some() && self.sftp_client.is_none());
        if needs_connection {
            match bookmark.host.as_deref() {
                Some(host) => self.connect_to_host(host).await?,
                None => {
                    self.sftp_client = None;
                    self.current_host = None;
                    self.remote_files.clear();
                }
            }
        }

        self.local_path = bookmark.local_path.clone();
        self.remote_path = bookmark.remote_path.clone();
        self.active_pane = bookmark.active_pane;
        self.reset_directory_view();
        self.refresh_local_files()?;
        self.refresh_remote_files().await?;
        Ok(())
    }

    async fn run_rsync(&self) -> Result<String> {
        let host = self
            .current_host
            .as_deref()
            .ok_or_else(|| anyhow!("connect to an SSH host before syncing"))?;
        if self.sftp_client.is_none() {
            return Err(anyhow!("connect to an SSH host before syncing"));
        }
        if !self.local_path.is_dir() {
            return Err(anyhow!(
                "local directory does not exist: {}",
                self.local_path.display()
            ));
        }

        let local = path_with_trailing_slash(&self.local_path);
        let remote = format!("{host}:{}", path_with_trailing_slash(&self.remote_path));
        let mut command = Command::new("rsync");
        command.args([
            "--archive",
            "--delete",
            "--checksum",
            "--itemize-changes",
            "--human-readable",
        ]);
        if self.sync_dry_run {
            command.arg("--dry-run");
        }

        match self.sync_direction {
            SyncDirection::LocalToRemote => {
                command.arg(&local).arg(&remote);
            }
            SyncDirection::RemoteToLocal => {
                command.arg(&remote).arg(&local);
            }
        }
        command.stdout(Stdio::piped()).stderr(Stdio::piped());

        let output = command
            .output()
            .await
            .map_err(|error| anyhow!("could not start rsync: {error}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let details = [stdout.trim(), stderr.trim()]
            .into_iter()
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");

        if !output.status.success() {
            return Err(anyhow!(
                "rsync exited with {}{}",
                output.status,
                if details.is_empty() {
                    String::new()
                } else {
                    format!(":\n{details}")
                }
            ));
        }

        if details.is_empty() {
            Ok("No changes.".to_string())
        } else {
            Ok(details)
        }
    }

    fn refresh_local_files(&mut self) -> Result<()> {
        self.local_files.clear();

        // Add parent directory entry if not at root
        if let Some(parent) = self.local_path.parent() {
            self.local_files.push(FileInfo {
                name: "..".to_string(),
                path: parent.to_path_buf(),
                is_dir: true,
                size: 0,
                permissions: 0o755,
            });
        }

        for entry in fs::read_dir(&self.local_path)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = entry.metadata()?;

            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("Unknown")
                .to_string();

            self.local_files.push(FileInfo {
                name,
                path,
                is_dir: metadata.is_dir(),
                size: metadata.len(),
                permissions: 0o755,
            });
        }

        // Sort with .. always first, then directories, then files
        self.local_files.sort_by(|a, b| {
            if a.name == ".." {
                std::cmp::Ordering::Less
            } else if b.name == ".." {
                std::cmp::Ordering::Greater
            } else {
                match (a.is_dir, b.is_dir) {
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    _ => a.name.cmp(&b.name),
                }
            }
        });

        self.local_cursor = 0;
        self.local_selected.clear();

        Ok(())
    }

    async fn refresh_remote_files(&mut self) -> Result<()> {
        if let Some(client) = &self.sftp_client {
            self.remote_files = client.list_directory(&self.remote_path)?;

            // Add parent directory entry if not at root
            if self.remote_path.as_path() != Path::new("/")
                && let Some(parent) = self.remote_path.parent()
            {
                self.remote_files.insert(
                    0,
                    FileInfo {
                        name: "..".to_string(),
                        path: parent.to_path_buf(),
                        is_dir: true,
                        size: 0,
                        permissions: 0o755,
                    },
                );
            }

            self.remote_cursor = 0;
            self.remote_selected.clear();
        }

        Ok(())
    }

    fn move_cursor_up(&mut self) {
        match self.active_pane {
            Pane::Local => {
                if self.local_cursor > 0 {
                    self.local_cursor -= 1;
                }
            }
            Pane::Remote => {
                if self.remote_cursor > 0 {
                    self.remote_cursor -= 1;
                }
            }
        }
    }

    fn move_cursor_down(&mut self) {
        match self.active_pane {
            Pane::Local => {
                let files_len = self.get_current_local_files().len();
                if self.local_cursor < files_len.saturating_sub(1) {
                    self.local_cursor += 1;
                }
            }
            Pane::Remote => {
                let files_len = self.get_current_remote_files().len();
                if self.remote_cursor < files_len.saturating_sub(1) {
                    self.remote_cursor += 1;
                }
            }
        }
    }

    fn move_cursor_top(&mut self) {
        match self.active_pane {
            Pane::Local => self.local_cursor = 0,
            Pane::Remote => self.remote_cursor = 0,
        }
    }

    fn move_cursor_bottom(&mut self) {
        match self.active_pane {
            Pane::Local => {
                self.local_cursor = self.get_current_local_files().len().saturating_sub(1);
            }
            Pane::Remote => {
                self.remote_cursor = self.get_current_remote_files().len().saturating_sub(1);
            }
        }
    }

    async fn go_to_parent_directory(&mut self) -> Result<()> {
        match self.active_pane {
            Pane::Local => {
                if let Some(parent) = self.local_path.parent()
                    && parent != self.local_path
                {
                    self.local_path = parent.to_path_buf();
                    self.reset_directory_view();
                    self.refresh_local_files()?;
                }
            }
            Pane::Remote => {
                if let Some(parent) = self.remote_path.parent()
                    && parent != self.remote_path
                {
                    self.remote_path = parent.to_path_buf();
                    self.reset_directory_view();
                    self.refresh_remote_files().await?;
                }
            }
        }

        Ok(())
    }

    fn reset_directory_view(&mut self) {
        self.search_mode = false;
        self.search_query.clear();
        self.clear_search_filter();
    }

    async fn change_directory(&mut self) -> Result<()> {
        match self.active_pane {
            Pane::Local => {
                let files = self.get_current_local_files();
                if let Some(file) = files.get(self.local_cursor)
                    && file.is_dir
                {
                    self.local_path = file.path.clone();
                    self.reset_directory_view();
                    self.refresh_local_files()?;
                }
            }
            Pane::Remote => {
                let files = self.get_current_remote_files();
                if let Some(file) = files.get(self.remote_cursor)
                    && file.is_dir
                {
                    self.remote_path = file.path.clone();
                    self.reset_directory_view();
                    self.refresh_remote_files().await?;
                }
            }
        }

        Ok(())
    }

    fn toggle_selection(&mut self) {
        match self.active_pane {
            Pane::Local => {
                if self.local_selected.contains(&self.local_cursor) {
                    self.local_selected.remove(&self.local_cursor);
                } else {
                    self.local_selected.insert(self.local_cursor);
                }
            }
            Pane::Remote => {
                if self.remote_selected.contains(&self.remote_cursor) {
                    self.remote_selected.remove(&self.remote_cursor);
                } else {
                    self.remote_selected.insert(self.remote_cursor);
                }
            }
        }
    }

    fn prepare_transfer(&mut self) -> Result<()> {
        self.transfer_queue.clear();

        for &index in &self.local_selected {
            if let Some(file) = self.local_files.get(index) {
                let destination = self.remote_path.join(&file.name);
                self.transfer_queue.push(TransferItem {
                    source: file.path.clone(),
                    destination,
                    direction: TransferDirection::Upload,
                });
            }
        }

        for &index in &self.remote_selected {
            if let Some(file) = self.remote_files.get(index) {
                let destination = self.local_path.join(&file.name);
                self.transfer_queue.push(TransferItem {
                    source: file.path.clone(),
                    destination,
                    direction: TransferDirection::Download,
                });
            }
        }

        if !self.transfer_queue.is_empty() {
            self.show_transfer_dialog = true;
        }

        Ok(())
    }

    async fn execute_transfers(&mut self) -> Result<()> {
        if let Some(client) = &self.sftp_client {
            for item in &self.transfer_queue {
                match item.direction {
                    TransferDirection::Upload => {
                        // Check if source is a directory
                        if item.source.is_dir() {
                            client.upload_directory(&item.source, &item.destination)?;
                        } else {
                            client.upload_file(&item.source, &item.destination)?;
                        }
                    }
                    TransferDirection::Download => {
                        client.download_file(&item.source, &item.destination)?;
                    }
                }
            }
        }

        self.transfer_queue.clear();
        self.local_selected.clear();
        self.remote_selected.clear();

        self.refresh_local_files()?;
        self.refresh_remote_files().await?;

        Ok(())
    }

    async fn handle_search_event(&mut self, key: KeyCode) -> Result<()> {
        match key {
            KeyCode::Esc => {
                self.search_mode = false;
                self.search_query.clear();
                self.clear_search_filter();
            }
            KeyCode::Enter => {
                self.search_mode = false;
            }
            KeyCode::Backspace => {
                self.search_query.pop();
                self.update_search_filter();
            }
            KeyCode::Char(c) => {
                self.search_query.push(c);
                self.update_search_filter();
            }
            _ => {}
        }

        Ok(())
    }

    fn start_search(&mut self) {
        self.search_mode = true;
        self.search_query.clear();
        self.clear_search_filter();
        self.local_cursor = 0;
        self.remote_cursor = 0;
    }

    fn update_search_filter(&mut self) {
        if self.search_query.is_empty() {
            self.clear_search_filter();
            return;
        }

        let query = self.search_query.to_lowercase();

        // Filter local files
        self.filtered_local_files = self
            .local_files
            .iter()
            .filter(|file| file.name.to_lowercase().contains(&query))
            .cloned()
            .collect();

        // Filter remote files
        self.filtered_remote_files = self
            .remote_files
            .iter()
            .filter(|file| file.name.to_lowercase().contains(&query))
            .cloned()
            .collect();
    }

    fn clear_search_filter(&mut self) {
        self.filtered_local_files.clear();
        self.filtered_remote_files.clear();
    }

    pub fn get_current_local_files(&self) -> &[FileInfo] {
        if self.search_mode && !self.search_query.is_empty() {
            &self.filtered_local_files
        } else {
            &self.local_files
        }
    }

    pub fn get_current_remote_files(&self) -> &[FileInfo] {
        if self.search_mode && !self.search_query.is_empty() {
            &self.filtered_remote_files
        } else {
            &self.remote_files
        }
    }
}

fn path_with_trailing_slash(path: &Path) -> String {
    let path = path.to_string_lossy();
    if path.ends_with('/') {
        path.into_owned()
    } else {
        format!("{}/", path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pane_toggle() {
        let pane = Pane::Local;
        assert_eq!(pane, Pane::Local);

        let pane = Pane::Remote;
        assert_eq!(pane, Pane::Remote);
    }

    #[test]
    fn test_transfer_item_upload() {
        let item = TransferItem {
            source: PathBuf::from("/source/file.txt"),
            destination: PathBuf::from("/dest/file.txt"),
            direction: TransferDirection::Upload,
        };

        assert_eq!(item.source, PathBuf::from("/source/file.txt"));
        assert_eq!(item.destination, PathBuf::from("/dest/file.txt"));
        assert!(matches!(item.direction, TransferDirection::Upload));
    }

    #[test]
    fn test_transfer_item_download() {
        let item = TransferItem {
            source: PathBuf::from("/remote/file.txt"),
            destination: PathBuf::from("/local/file.txt"),
            direction: TransferDirection::Download,
        };

        assert!(matches!(item.direction, TransferDirection::Download));
    }

    #[test]
    fn test_transfer_direction_clone() {
        let upload = TransferDirection::Upload;
        let cloned = upload.clone();
        assert!(matches!(cloned, TransferDirection::Upload));
    }

    #[test]
    fn test_path_with_trailing_slash() {
        assert_eq!(
            path_with_trailing_slash(Path::new("/tmp/source")),
            "/tmp/source/"
        );
        assert_eq!(path_with_trailing_slash(Path::new("/")), "/");
    }

    #[test]
    fn test_bookmark_round_trip() {
        let bookmark = Bookmark {
            name: "project".to_string(),
            local_path: PathBuf::from("/tmp/project"),
            remote_path: PathBuf::from("/srv/project"),
            host: Some("server".to_string()),
            active_pane: Pane::Remote,
        };

        let encoded = serde_json::to_string(&bookmark).unwrap();
        let decoded: Bookmark = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.name, bookmark.name);
        assert_eq!(decoded.local_path, bookmark.local_path);
        assert_eq!(decoded.remote_path, bookmark.remote_path);
        assert_eq!(decoded.host, bookmark.host);
        assert_eq!(decoded.active_pane, Pane::Remote);
    }
}
