use crate::constants::{AUDIT_IGNORED_PATH_PREFIXES, AUDIT_LOG_PATH, LD_SO_CACHE};
use crate::errors::{Error, Result};
use std::path::PathBuf;
use std::process::Command;

/// Report generated by audit mode showing actual access patterns
#[derive(Debug, Clone)]
pub struct AuditReport {
    /// Files that were accessed during execution
    pub accessed_files: Vec<String>,
    /// Network connections that were attempted
    pub network_connections: Vec<String>,
}

impl AuditReport {
    /// Print a human-readable summary of the audit report
    pub fn print_summary(&self) {
        println!("🔍 Audit Report:");
        println!("================");

        if !self.accessed_files.is_empty() {
            println!(
                "\n📁 File Access ({} unique paths):",
                self.accessed_files.len()
            );
            for file in &self.accessed_files {
                println!("  • {file}");
            }
        }

        if !self.network_connections.is_empty() {
            println!(
                "\n🌐 Network Access ({} unique connections):",
                self.network_connections.len()
            );
            for conn in &self.network_connections {
                println!("  • {conn}");
            }
        }

        if self.accessed_files.is_empty() && self.network_connections.is_empty() {
            println!("  No file or network access detected");
        }

        println!("\n💡 Recommendations:");
        if !self.accessed_files.is_empty() {
            println!("  Add to security.readOnlyPaths or security.readWritePaths:");
            for file in &self.accessed_files {
                println!("    - \"{file}\"");
            }
        }

        if !self.network_connections.is_empty() {
            println!("  Add to security.allowedHosts:");
            for conn in &self.network_connections {
                println!("    - \"{conn}\"");
            }
        }
    }
}

/// Configuration for access restrictions when running commands
#[derive(Debug, Clone, Default)]
pub struct AccessRestrictions {
    /// Restrict disk access (filesystem operations)
    pub restrict_disk: bool,
    /// Restrict network access (network connections)
    pub restrict_network: bool,
    /// Paths that are allowed for reading
    pub read_only_paths: Vec<PathBuf>,
    /// Paths that are allowed for reading and writing
    pub read_write_paths: Vec<PathBuf>,
    /// Paths that are explicitly denied
    pub deny_paths: Vec<PathBuf>,
    /// Allowed network hosts/CIDRs (empty means block all)
    pub allowed_hosts: Vec<String>,
    /// Audit mode - collect access information instead of restricting
    pub audit_mode: bool,
}

impl AccessRestrictions {
    /// Check if Landlock is supported on the current system
    #[cfg(target_os = "linux")]
    pub fn is_landlock_supported() -> bool {
        // For now, we'll check by trying to create a ruleset
        use landlock::Ruleset;
        Ruleset::default().create().is_ok()
    }

    #[cfg(not(target_os = "linux"))]
    pub fn is_landlock_supported() -> bool {
        false
    }
    /// Create new restrictions configuration
    pub fn new(restrict_disk: bool, restrict_network: bool) -> Self {
        Self {
            restrict_disk,
            restrict_network,
            read_only_paths: Vec::new(),
            read_write_paths: Vec::new(),
            deny_paths: Vec::new(),
            allowed_hosts: Vec::new(),
            audit_mode: false,
        }
    }

    /// Create restrictions with explicit path and network allowlists
    pub fn with_allowlists(
        restrict_disk: bool,
        restrict_network: bool,
        read_only_paths: Vec<PathBuf>,
        read_write_paths: Vec<PathBuf>,
        deny_paths: Vec<PathBuf>,
        allowed_hosts: Vec<String>,
    ) -> Self {
        Self {
            restrict_disk,
            restrict_network,
            read_only_paths,
            read_write_paths,
            deny_paths,
            allowed_hosts,
            audit_mode: false,
        }
    }

    /// Add a read-only path to the allowlist
    pub fn add_read_only_path<P: Into<PathBuf>>(&mut self, path: P) {
        self.read_only_paths.push(path.into());
    }

    /// Add a read-write path to the allowlist
    pub fn add_read_write_path<P: Into<PathBuf>>(&mut self, path: P) {
        self.read_write_paths.push(path.into());
    }

    /// Add a denied path
    pub fn add_deny_path<P: Into<PathBuf>>(&mut self, path: P) {
        self.deny_paths.push(path.into());
    }

    /// Enable audit mode
    pub fn enable_audit_mode(&mut self) {
        self.audit_mode = true;
    }

    /// Run command with audit monitoring using strace
    pub fn run_with_audit(&self, cmd: &mut Command) -> Result<(i32, AuditReport)> {
        if !cfg!(target_os = "linux") {
            return Err(Error::configuration(
                "Audit mode is only supported on Linux systems".to_string(),
            ));
        }

        // Use strace to monitor file and network access
        let mut strace_cmd = Command::new("strace");
        strace_cmd
            .arg("-f") // Follow forks
            .arg("-e") // Filter system calls
            .arg("trace=file,network") // Monitor file and network calls
            .arg("-o") // Output to file
            .arg(AUDIT_LOG_PATH)
            .arg("--");

        // Add the original command and arguments
        if let Some(program) = cmd.get_program().to_str() {
            strace_cmd.arg(program);
        }

        for arg in cmd.get_args() {
            if let Some(arg_str) = arg.to_str() {
                strace_cmd.arg(arg_str);
            }
        }

        // Copy environment and working directory
        strace_cmd.envs(cmd.get_envs().filter_map(|(k, v)| Some((k, v?))));

        if let Some(current_dir) = cmd.get_current_dir() {
            strace_cmd.current_dir(current_dir);
        }

        // Execute with strace
        let output = strace_cmd.output().map_err(|e| {
            Error::command_execution(
                "strace",
                vec!["monitoring command".to_string()],
                format!("Failed to run audit command: {e}"),
                None,
            )
        })?;

        // Parse the audit log
        let audit_report = self.parse_audit_log(AUDIT_LOG_PATH)?;

        // Clean up the audit log
        let _ = std::fs::remove_file(AUDIT_LOG_PATH);

        Ok((output.status.code().unwrap_or(1), audit_report))
    }

    /// Parse strace output to generate audit report
    fn parse_audit_log(&self, log_path: &str) -> Result<AuditReport> {
        use std::collections::HashSet;
        use std::fs;

        let mut accessed_files = HashSet::new();
        let mut network_connections = HashSet::new();

        if let Ok(content) = fs::read_to_string(log_path) {
            for line in content.lines() {
                // Parse file access patterns
                if line.contains("openat(") || line.contains("open(") {
                    if let Some(path) = extract_file_path(line) {
                        accessed_files.insert(path);
                    }
                }

                // Parse network connection patterns
                if line.contains("connect(") || line.contains("bind(") {
                    if let Some(addr) = extract_network_address(line) {
                        network_connections.insert(addr);
                    }
                }
            }
        }

        Ok(AuditReport {
            accessed_files: accessed_files.into_iter().collect(),
            network_connections: network_connections.into_iter().collect(),
        })
    }

    /// Add an allowed network host/CIDR
    pub fn add_allowed_host<S: Into<String>>(&mut self, host: S) {
        self.allowed_hosts.push(host.into());
    }

    /// Create AccessRestrictions from a SecurityConfig
    pub fn from_security_config(security: &crate::cue_parser::SecurityConfig) -> Self {
        use std::path::PathBuf;

        Self {
            restrict_disk: security.restrict_disk.unwrap_or(false),
            restrict_network: security.restrict_network.unwrap_or(false),
            read_only_paths: security
                .read_only_paths
                .as_ref()
                .map(|paths| paths.iter().map(PathBuf::from).collect())
                .unwrap_or_default(),
            read_write_paths: security
                .read_write_paths
                .as_ref()
                .map(|paths| paths.iter().map(PathBuf::from).collect())
                .unwrap_or_default(),
            deny_paths: security
                .deny_paths
                .as_ref()
                .map(|paths| paths.iter().map(PathBuf::from).collect())
                .unwrap_or_default(),
            allowed_hosts: security.allowed_hosts.as_ref().cloned().unwrap_or_default(),
            audit_mode: false,
        }
    }

    /// Create AccessRestrictions from a SecurityConfig with optional task context for inference
    pub fn from_security_config_with_task(
        security: &crate::cue_parser::SecurityConfig,
        task_config: &crate::cue_parser::TaskConfig,
    ) -> Self {
        use std::path::PathBuf;

        let mut restrictions = Self::from_security_config(security);

        // If inference is enabled, add paths from inputs and outputs
        if security.infer_from_inputs_outputs.unwrap_or(false) {
            // Add inputs as read-only paths
            if let Some(inputs) = &task_config.inputs {
                for input in inputs {
                    restrictions.add_read_only_path(PathBuf::from(input));
                }
            }

            // Add outputs as read-write paths
            if let Some(outputs) = &task_config.outputs {
                for output in outputs {
                    restrictions.add_read_write_path(PathBuf::from(output));
                }
            }

            // If we have inputs or outputs, enable disk restrictions automatically
            if task_config.inputs.is_some() || task_config.outputs.is_some() {
                restrictions.restrict_disk = true;
            }
        }

        restrictions
    }

    /// Apply restrictions to a command before execution
    /// This is the main entry point for applying platform-specific restrictions
    pub fn apply_to_command(&self, cmd: &mut Command) -> Result<()> {
        if !self.has_any_restrictions() {
            return Ok(());
        }
        // Apply platform-specific restrictions
        #[cfg(target_os = "linux")]
        self.apply_landlock_restrictions(cmd)?;

        #[cfg(not(target_os = "linux"))]
        self.apply_fallback_restrictions(cmd)?;

        Ok(())
    }

    /// Check if any restrictions are enabled
    pub fn has_any_restrictions(&self) -> bool {
        self.restrict_disk || self.restrict_network
    }

    /// Apply Landlock-based restrictions on Linux
    #[cfg(target_os = "linux")]
    fn apply_landlock_restrictions(&self, cmd: &mut Command) -> Result<()> {
        use landlock::{
            Access, AccessFs, AccessNet, NetPort, PathBeneath, PathFd, Ruleset, RulesetAttr,
            RulesetCreatedAttr, RulesetStatus, ABI,
        };
        use std::os::unix::process::CommandExt;

        // Clone the necessary data for the pre_exec closure
        let restrict_disk = self.restrict_disk;
        let restrict_network = self.restrict_network;
        let read_only_paths = self.read_only_paths.clone();
        let read_write_paths = self.read_write_paths.clone();
        let allowed_hosts = self.allowed_hosts.clone();

        // SAFETY: The pre_exec closure is only executed in the child process after fork()
        // but before exec(). The cloned data is moved into the closure, ensuring it
        // remains valid for the duration of the closure execution. The closure does not
        // access any mutable state from the parent process and only performs system calls
        // to set up Landlock restrictions. If the closure returns an error, the child
        // process will terminate without executing the target command.
        unsafe {
            cmd.pre_exec(move || {
                // Use the highest ABI we want to support
                let abi = ABI::V4;

                log::debug!("Applying Landlock restrictions in child process");

                // Build the ruleset
                let mut ruleset = Ruleset::default();

                // Add filesystem access handling if needed
                if restrict_disk {
                    let handled_fs = AccessFs::from_all(abi);
                    ruleset = ruleset.handle_access(handled_fs).map_err(|e| {
                        std::io::Error::other(format!(
                            "Failed to configure filesystem access handling: {e}"
                        ))
                    })?;
                }

                // Add network access handling if needed (V2 or higher required)
                if restrict_network {
                    // Handle both TCP bind and connect
                    ruleset = ruleset
                        .handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp)
                        .map_err(|e| {
                            std::io::Error::other(format!(
                                "Failed to configure network access handling: {e}"
                            ))
                        })?;
                }

                // Create the ruleset
                let mut ruleset = ruleset.create().map_err(|e| {
                    std::io::Error::other(format!("Failed to create Landlock ruleset: {e}"))
                })?;

                // Add filesystem rules
                if restrict_disk {
                    // Add read-only paths
                    for path in &read_only_paths {
                        if let Ok(path_fd) = PathFd::new(path) {
                            let read_access =
                                AccessFs::ReadFile | AccessFs::ReadDir | AccessFs::Execute;
                            let rule = PathBeneath::new(path_fd, read_access);
                            ruleset = ruleset.add_rule(rule).map_err(|e| {
                                std::io::Error::other(format!(
                                    "Failed to add read-only rule for {}: {e}",
                                    path.display()
                                ))
                            })?;
                        }
                    }

                    // Add read-write paths
                    for path in &read_write_paths {
                        if let Ok(path_fd) = PathFd::new(path) {
                            let rule = PathBeneath::new(path_fd, AccessFs::from_all(abi));
                            ruleset = ruleset.add_rule(rule).map_err(|e| {
                                std::io::Error::other(format!(
                                    "Failed to add read-write rule for {}: {e}",
                                    path.display()
                                ))
                            })?;
                        }
                    }
                }

                // Add network rules (requires ABI V2 or higher)
                if restrict_network {
                    // Parse and add allowed hosts (ports)
                    for host in &allowed_hosts {
                        // Try to parse as port number
                        if let Ok(port) = host.parse::<u16>() {
                            // Allow both bind and connect on this port
                            let bind_rule = NetPort::new(port, AccessNet::BindTcp);
                            let connect_rule = NetPort::new(port, AccessNet::ConnectTcp);

                            ruleset = ruleset.add_rule(bind_rule).map_err(|e| {
                                std::io::Error::other(format!(
                                    "Failed to add bind rule for port {port}: {e}"
                                ))
                            })?;

                            ruleset = ruleset.add_rule(connect_rule).map_err(|e| {
                                std::io::Error::other(format!(
                                    "Failed to add connect rule for port {port}: {e}"
                                ))
                            })?;
                        }
                    }
                }

                // Apply the restrictions to this process (which will be the child)
                let status = ruleset.restrict_self().map_err(|e| {
                    std::io::Error::other(format!("Failed to apply Landlock restrictions: {e}"))
                })?;

                if status.ruleset == RulesetStatus::NotEnforced {
                    return Err(std::io::Error::other(
                        "Landlock is not supported by the running kernel.",
                    ));
                }

                Ok(())
            });
        }

        Ok(())
    }

    /// Apply fallback restrictions on non-Linux platforms
    #[cfg(not(target_os = "linux"))]
    fn apply_fallback_restrictions(&self, _cmd: &mut Command) -> Result<()> {
        if self.has_any_restrictions() {
            return Err(Error::configuration(
                "Access restrictions are only supported on Linux with Landlock. Please use a Linux system with kernel 5.13+ for sandboxing support.".to_string()
            ));
        }
        Ok(())
    }
}

/// Extract file path from strace output line
fn extract_file_path(line: &str) -> Option<String> {
    // Look for patterns like: openat(AT_FDCWD, "/path/to/file", O_RDONLY) = 3
    if let Some(start) = line.find('"') {
        if let Some(end) = line[start + 1..].find('"') {
            let path = &line[start + 1..start + 1 + end];
            // Filter out common system paths that aren't interesting for user restrictions
            if !AUDIT_IGNORED_PATH_PREFIXES
                .iter()
                .any(|prefix| path.starts_with(prefix))
                && path != LD_SO_CACHE
            {
                return Some(path.to_string());
            }
        }
    }
    None
}

/// Extract network address from strace output line
fn extract_network_address(line: &str) -> Option<String> {
    // Look for patterns like: connect(3, {sa_family=AF_INET, sin_port=htons(80), sin_addr=inet_addr("127.0.0.1")}, 16) = 0
    if line.contains("AF_INET") {
        if let Some(addr_start) = line.find("inet_addr(\"") {
            if let Some(addr_end) = line[addr_start + 11..].find('"') {
                let addr = &line[addr_start + 11..addr_start + 11 + addr_end];
                return Some(addr.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_access_restrictions_creation() {
        let restrictions = AccessRestrictions::new(true, true);
        assert!(restrictions.restrict_disk);
        assert!(restrictions.restrict_network);
        assert!(restrictions.has_any_restrictions());
    }

    #[test]
    fn test_access_restrictions_with_allowlists() {
        let restrictions = AccessRestrictions::with_allowlists(
            true,
            true,
            vec![PathBuf::from("/tmp")],
            vec![PathBuf::from("/var/tmp")],
            vec![PathBuf::from("/etc/passwd")],
            vec!["localhost".to_string()],
        );

        assert!(restrictions.restrict_disk);
        assert!(restrictions.restrict_network);
        assert_eq!(restrictions.read_only_paths.len(), 1);
        assert_eq!(restrictions.read_write_paths.len(), 1);
        assert_eq!(restrictions.deny_paths.len(), 1);
        assert_eq!(restrictions.allowed_hosts.len(), 1);
    }

    #[test]
    fn test_no_restrictions() {
        let restrictions = AccessRestrictions::default();
        assert!(!restrictions.has_any_restrictions());
    }

    #[test]
    fn test_add_paths_and_hosts() {
        let mut restrictions = AccessRestrictions::new(true, true);
        restrictions.add_read_only_path("/usr/lib");
        restrictions.add_read_write_path("/tmp");
        restrictions.add_deny_path("/etc/shadow");
        restrictions.add_allowed_host("example.com");

        assert_eq!(restrictions.read_only_paths.len(), 1);
        assert_eq!(restrictions.read_write_paths.len(), 1);
        assert_eq!(restrictions.deny_paths.len(), 1);
        assert_eq!(restrictions.allowed_hosts.len(), 1);
    }

    #[test]
    fn test_apply_to_command_no_restrictions() {
        let restrictions = AccessRestrictions::default();
        let mut cmd = Command::new("echo");
        cmd.arg("test");

        let result = restrictions.apply_to_command(&mut cmd);
        assert!(result.is_ok());

        // Command should be unchanged
        assert_eq!(cmd.get_program(), "echo");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_landlock_restrictions_available() {
        let mut restrictions = AccessRestrictions::new(true, false);
        restrictions.add_read_only_path("/tmp");
        restrictions.add_read_write_path("/var/tmp");

        let mut cmd = Command::new("echo");
        cmd.arg("test");

        // This might fail if Landlock is not available, but shouldn't panic
        let _result = restrictions.apply_to_command(&mut cmd);
        // We can't easily test the actual Landlock functionality without kernel support
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn test_non_linux_restrictions_fail() {
        let restrictions = AccessRestrictions::new(true, false);
        let mut cmd = Command::new("echo");
        cmd.arg("test");

        let result = restrictions.apply_to_command(&mut cmd);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("only supported on Linux"));
    }
}
