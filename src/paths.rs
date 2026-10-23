use std::path::PathBuf;

/// Where Tugport keeps its files. Overridable for development and tests.
#[derive(Clone, Debug)]
pub struct Paths {
    /// Config and the master key (`/etc/tugport`).
    pub etc: PathBuf,
    /// Database, logs, sources, build cache (`/var/lib/tugport`).
    pub data: PathBuf,
    /// Memory-only runtime files such as delivered secrets (`/run/tugport`).
    pub run: PathBuf,
}

impl Paths {
    pub fn from_env() -> Self {
        let get = |var: &str, default: &str| PathBuf::from(std::env::var(var).unwrap_or_else(|_| default.into()));
        Paths {
            etc: get("TUGPORT_ETC", "/etc/tugport"),
            data: get("TUGPORT_DATA", "/var/lib/tugport"),
            run: get("TUGPORT_RUN", "/run/tugport"),
        }
    }

    pub fn db(&self) -> PathBuf {
        self.data.join("tugport.db")
    }
    pub fn logs(&self) -> PathBuf {
        self.data.join("logs")
    }
    pub fn sources(&self) -> PathBuf {
        self.data.join("src")
    }
    pub fn deploy_logs(&self) -> PathBuf {
        self.data.join("deploys")
    }
    pub fn tools(&self) -> PathBuf {
        self.data.join("bin")
    }
    pub fn secrets(&self) -> PathBuf {
        self.run.join("secrets")
    }
    /// Secret files mounted into containers, one directory per container.
    pub fn files(&self) -> PathBuf {
        self.run.join("files")
    }
    /// Static copy of this binary that containers mount as their init.
    pub fn init_binary(&self) -> PathBuf {
        self.data.join("bin").join("tugport-init")
    }

    pub fn ensure(&self) -> std::io::Result<()> {
        for dir in [&self.etc, &self.data, &self.logs(), &self.sources(), &self.deploy_logs(), &self.tools()] {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::create_dir_all(self.secrets())?;
        std::fs::create_dir_all(self.files())?;
        restrict(&self.etc, 0o700)?;
        // The database, container and deploy logs and Git checkouts (private code): root only.
        // Containers reach the files mounted into them (the init, Compose folders) regardless.
        restrict(&self.data, 0o700)?;
        restrict(&self.run, 0o711)?;
        // Only root may walk to delivered secrets; containers reach theirs through bind mounts.
        restrict(&self.secrets(), 0o700)?;
        restrict(&self.files(), 0o700)?;
        Ok(())
    }
}

pub fn restrict(path: &std::path::Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}
