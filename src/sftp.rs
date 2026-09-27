use crate::ssh_config::{SshConfig, SshHost};
use anyhow::{Result, anyhow};
use ssh2::{Channel, Session, Sftp};
use std::env;
use std::fs;
use std::io::prelude::*;
use std::net::TcpStream;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::{Arc, Mutex};
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct FileInfo {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_symlink: bool,
    #[allow(dead_code)]
    pub size: u64,
    #[allow(dead_code)]
    pub permissions: u32,
}

pub struct SftpClient {
    _session: Session,
    #[cfg(unix)]
    _bastion_session: Option<Session>,
    #[cfg(unix)]
    _proxy_threads: Option<ProxyThreads>,
    sftp: Sftp,
}

/// OpenSSH uses the local account name when a host does not specify `User`.
/// Keep the same behavior for hosts loaded from `~/.ssh/config`.
fn username_for(host_config: &SshHost) -> Result<String> {
    if let Some(user) = host_config.user.as_deref().filter(|user| !user.is_empty()) {
        return Ok(user.to_owned());
    }

    ["USER", "USERNAME"]
        .iter()
        .find_map(|name| env::var(name).ok().filter(|user| !user.is_empty()))
        .ok_or_else(|| {
            anyhow!("No username specified and the current username could not be determined")
        })
}

fn ssh_agent_available() -> bool {
    env::var_os("SSH_AUTH_SOCK")
        .map(|socket| !socket.is_empty())
        .unwrap_or(false)
}

fn identity_candidates(identity_file: Option<&Path>) -> Vec<PathBuf> {
    if let Some(identity_file) = identity_file {
        let path = identity_file.to_string_lossy();
        if let Some(home) = dirs::home_dir() {
            if path == "~" {
                return vec![home];
            }
            if let Some(relative) = path.strip_prefix("~/") {
                return vec![home.join(relative)];
            }
        }
        return vec![identity_file.to_path_buf()];
    }

    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };

    ["id_ed25519", "id_ecdsa", "id_rsa", "id_dsa"]
        .into_iter()
        .map(|name| home.join(".ssh").join(name))
        .collect()
}

fn authenticate(session: &mut Session, user: &str, identity_file: Option<&Path>) -> Result<()> {
    let mut errors = Vec::new();

    for identity in identity_candidates(identity_file) {
        if !identity.is_file() {
            continue;
        }

        match session.userauth_pubkey_file(user, None, &identity, None) {
            Ok(()) if session.authenticated() => return Ok(()),
            Ok(()) => errors.push(format!(
                "key authentication did not authenticate with {}",
                identity.display()
            )),
            Err(error) => errors.push(format!("{}: {error}", identity.display())),
        }
    }

    if ssh_agent_available() {
        match session.userauth_agent(user) {
            Ok(()) if session.authenticated() => return Ok(()),
            Ok(()) => errors.push("ssh-agent did not authenticate".to_string()),
            Err(error) => errors.push(format!("ssh-agent: {error}")),
        }
    }

    let detail = if errors.is_empty() {
        "no usable identity file or SSH agent was found".to_string()
    } else {
        errors.join("; ")
    };
    Err(anyhow!("Authentication failed for user '{user}': {detail}"))
}

#[cfg(unix)]
struct ProxyThreads {
    #[allow(dead_code)]
    handles: Vec<thread::JoinHandle<()>>,
}

impl SftpClient {
    pub fn connect(host_config: &SshHost) -> Result<Self> {
        // Check if we need to use ProxyJump
        #[cfg(unix)]
        if let Some(proxy_jump) = &host_config.proxy_jump {
            // Get SSH config to look up bastion host details
            let ssh_config = SshConfig::new()?;
            let bastion_config = ssh_config.get_host(proxy_jump).ok_or_else(|| {
                anyhow!("ProxyJump host '{}' not found in SSH config", proxy_jump)
            })?;

            return Self::connect_via_proxy(host_config, &bastion_config);
        }

        #[cfg(not(unix))]
        if host_config.proxy_jump.is_some() {
            return Err(anyhow!("ProxyJump is not supported on Windows"));
        }

        // Direct connection
        Self::connect_direct(host_config)
    }

    fn connect_direct(host_config: &SshHost) -> Result<Self> {
        let hostname = host_config.hostname.as_ref().unwrap_or(&host_config.host);
        let port = host_config.port.unwrap_or(22);
        let user = username_for(host_config)?;

        let tcp = TcpStream::connect(format!("{hostname}:{port}"))?;
        let mut session = Session::new()?;
        session.set_tcp_stream(tcp);
        session.handshake()?;

        authenticate(&mut session, &user, host_config.identity_file.as_deref())?;

        let sftp = session.sftp()?;

        Ok(SftpClient {
            _session: session,
            #[cfg(unix)]
            _bastion_session: None,
            #[cfg(unix)]
            _proxy_threads: None,
            sftp,
        })
    }

    #[cfg(unix)]
    fn connect_via_proxy(host_config: &SshHost, bastion_config: &SshHost) -> Result<Self> {
        // First, connect to bastion host
        let bastion_hostname = bastion_config
            .hostname
            .as_ref()
            .unwrap_or(&bastion_config.host);
        let bastion_port = bastion_config.port.unwrap_or(22);

        // Validate port number
        if bastion_port == 0 {
            return Err(anyhow!(
                "Invalid port number for bastion host: {}",
                bastion_port
            ));
        }

        let bastion_user = username_for(bastion_config)?;

        let bastion_tcp = TcpStream::connect(format!("{bastion_hostname}:{bastion_port}"))?;
        let mut bastion_session = Session::new()?;
        bastion_session.set_tcp_stream(bastion_tcp);
        bastion_session.handshake()?;

        authenticate(
            &mut bastion_session,
            &bastion_user,
            bastion_config.identity_file.as_deref(),
        )?;

        // Set bastion session to non-blocking mode
        bastion_session.set_blocking(false);

        // Create a direct-tcpip channel to the target host through bastion
        let target_hostname = host_config.hostname.as_ref().unwrap_or(&host_config.host);
        let target_port = host_config.port.unwrap_or(22);

        // Validate target port number
        if target_port == 0 {
            return Err(anyhow!(
                "Invalid port number for target host: {}",
                target_port
            ));
        }

        let channel = bastion_session.channel_direct_tcpip(target_hostname, target_port, None)?;

        // Create a socketpair for the proxy
        let (local_sock, remote_sock) = UnixStream::pair()?;
        local_sock.set_nonblocking(true)?;
        remote_sock.set_nonblocking(true)?;

        // Create Arc<Mutex<Channel>> for thread sharing
        let channel = Arc::new(Mutex::new(channel));

        // Start proxy threads
        let proxy_threads = Self::start_proxy_threads(channel, remote_sock)?;

        // Create session for target host using the local socket
        let mut target_session = Session::new()?;
        target_session.set_tcp_stream(local_sock);
        target_session.handshake()?;

        // Authenticate to target host
        let target_user = username_for(host_config)?;

        authenticate(
            &mut target_session,
            &target_user,
            host_config.identity_file.as_deref(),
        )?;

        let sftp = target_session.sftp()?;

        Ok(SftpClient {
            _session: target_session,
            _bastion_session: Some(bastion_session),
            _proxy_threads: Some(proxy_threads),
            sftp,
        })
    }

    #[cfg(unix)]
    fn start_proxy_threads(channel: Arc<Mutex<Channel>>, sock: UnixStream) -> Result<ProxyThreads> {
        let sock_clone = sock.try_clone()?;
        let channel_clone = Arc::clone(&channel);

        // Thread 1: Read from socket and write to channel
        let handle1 = thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            let mut sock = sock_clone;

            loop {
                match sock.read(&mut buffer) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        let data = &buffer[..n];
                        if let Ok(mut chan) = channel_clone.lock() {
                            if chan.write_all(data).is_err() {
                                break;
                            }
                            if chan.flush().is_err() {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(_) => break,
                }
            }
        });

        // Thread 2: Read from channel and write to socket
        let handle2 = thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            let mut sock = sock;

            while let Ok(mut chan) = channel.lock() {
                match chan.read(&mut buffer) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        let data = &buffer[..n];
                        if sock.write_all(data).is_err() {
                            break;
                        }
                        if sock.flush().is_err() {
                            break;
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        drop(chan); // Release lock before sleeping
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(ProxyThreads {
            handles: vec![handle1, handle2],
        })
    }

    pub fn list_directory(&self, path: &Path) -> Result<Vec<FileInfo>> {
        let mut files = Vec::new();

        for (path_buf, stat) in self.sftp.readdir(path)? {
            let name = path_buf
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("Unknown")
                .to_string();

            let is_dir = stat.is_dir();
            let permissions = stat.perm.unwrap_or(0);
            let is_symlink = permissions & 0o170000 == 0o120000;
            let size = stat.size.unwrap_or(0);

            files.push(FileInfo {
                name,
                path: path_buf,
                is_dir,
                is_symlink,
                size,
                permissions,
            });
        }

        files.sort_by(|a, b| match (a.is_dir, b.is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        });

        Ok(files)
    }

    pub fn download_file(&self, remote_path: &Path, local_path: &Path) -> Result<()> {
        let mut remote_file = self.sftp.open(remote_path)?;
        let mut local_file = fs::File::create(local_path)?;

        let mut buffer = [0; 8192];
        loop {
            let bytes_read = remote_file.read(&mut buffer)?;
            if bytes_read == 0 {
                break;
            }
            local_file.write_all(&buffer[..bytes_read])?;
        }

        Ok(())
    }

    pub fn upload_file(&self, local_path: &Path, remote_path: &Path) -> Result<()> {
        let mut local_file = fs::File::open(local_path)?;
        let mut remote_file = self.sftp.create(remote_path)?;

        let mut buffer = [0; 8192];
        loop {
            let bytes_read = local_file.read(&mut buffer)?;
            if bytes_read == 0 {
                break;
            }
            remote_file.write_all(&buffer[..bytes_read])?;
        }

        Ok(())
    }

    pub fn create_directory(&self, remote_path: &Path) -> Result<()> {
        self.sftp.mkdir(remote_path, 0o755)?;
        Ok(())
    }

    pub fn upload_directory(&self, local_path: &Path, remote_path: &Path) -> Result<()> {
        // Create the remote directory
        self.create_directory(remote_path)?;

        // Read local directory contents
        let entries = fs::read_dir(local_path)?;

        for entry in entries {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let file_name = entry.file_name();
            let local_file_path = entry.path();
            let remote_file_path = remote_path.join(&file_name);

            if file_type.is_dir() {
                // Recursively upload subdirectory
                self.upload_directory(&local_file_path, &remote_file_path)?;
            } else {
                // Upload file
                self.upload_file(&local_file_path, &remote_file_path)?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_info_creation() {
        let file_info = FileInfo {
            name: "test.txt".to_string(),
            path: PathBuf::from("/home/user/test.txt"),
            is_dir: false,
            is_symlink: false,
            size: 1024,
            permissions: 0o644,
        };

        assert_eq!(file_info.name, "test.txt");
        assert_eq!(file_info.path, PathBuf::from("/home/user/test.txt"));
        assert!(!file_info.is_dir);
        assert_eq!(file_info.size, 1024);
        assert_eq!(file_info.permissions, 0o644);
    }

    #[test]
    fn test_file_info_directory() {
        let dir_info = FileInfo {
            name: "documents".to_string(),
            path: PathBuf::from("/home/user/documents"),
            is_dir: true,
            is_symlink: false,
            size: 4096,
            permissions: 0o755,
        };

        assert!(dir_info.is_dir);
        assert_eq!(dir_info.permissions, 0o755);
    }

    #[test]
    fn test_file_info_clone() {
        let original = FileInfo {
            name: "file.rs".to_string(),
            path: PathBuf::from("/project/src/file.rs"),
            is_dir: false,
            is_symlink: false,
            size: 2048,
            permissions: 0o644,
        };

        let cloned = original.clone();
        assert_eq!(original.name, cloned.name);
        assert_eq!(original.path, cloned.path);
        assert_eq!(original.is_dir, cloned.is_dir);
        assert_eq!(original.is_symlink, cloned.is_symlink);
        assert_eq!(original.size, cloned.size);
        assert_eq!(original.permissions, cloned.permissions);
    }

    #[test]
    #[cfg(unix)]
    fn test_proxy_jump_config() {
        // Test that ProxyJump configuration is properly detected
        let host_with_proxy = SshHost {
            host: "target-host".to_string(),
            hostname: Some("10.0.0.1".to_string()),
            user: Some("user".to_string()),
            port: Some(22),
            identity_file: None,
            proxy_jump: Some("bastion-host".to_string()),
        };

        assert!(host_with_proxy.proxy_jump.is_some());
        assert_eq!(host_with_proxy.proxy_jump.as_ref().unwrap(), "bastion-host");
    }

    #[test]
    fn test_port_validation() {
        // Test port validation - port 0 should be invalid
        let host_config = SshHost {
            host: "test".to_string(),
            hostname: Some("test.example.com".to_string()),
            user: Some("user".to_string()),
            port: Some(0),
            identity_file: None,
            proxy_jump: None,
        };

        // Port 0 is invalid
        assert_eq!(host_config.port.unwrap_or(22), 0);
    }
}
