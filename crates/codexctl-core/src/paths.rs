use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct PathEnvironment {
    xdg_config_home: Option<PathBuf>,
    xdg_state_home: Option<PathBuf>,
    home: Option<PathBuf>,
}

impl PathEnvironment {
    pub fn new(
        xdg_config_home: Option<PathBuf>,
        xdg_state_home: Option<PathBuf>,
        home: Option<PathBuf>,
    ) -> Self {
        Self {
            xdg_config_home,
            xdg_state_home,
            home,
        }
    }

    pub fn current() -> Self {
        Self::new(
            std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
            std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
            std::env::var_os("HOME").map(PathBuf::from),
        )
    }

    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    pub fn xdg_config_home(&self) -> Option<&Path> {
        self.xdg_config_home.as_deref()
    }

    pub fn xdg_state_home(&self) -> Option<&Path> {
        self.xdg_state_home.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodingBrainPaths {
    config_file: PathBuf,
    state_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    MissingHome,
    RelativeBase(&'static str),
}

impl CodingBrainPaths {
    pub fn resolve(env: &PathEnvironment) -> Result<Self, PathError> {
        let config_base = resolve_base(
            env.xdg_config_home.as_deref(),
            env.home.as_deref(),
            "XDG_CONFIG_HOME",
            ".config",
        )?;
        let state_base = resolve_base(
            env.xdg_state_home.as_deref(),
            env.home.as_deref(),
            "XDG_STATE_HOME",
            ".local/state",
        )?;
        Ok(Self {
            config_file: config_base.join("coding-brain/config.toml"),
            state_root: state_base.join("coding-brain"),
        })
    }

    pub fn config_file(&self) -> &Path {
        &self.config_file
    }

    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    pub fn project_config(&self, cwd: &Path) -> PathBuf {
        cwd.join(".coding-brain.toml")
    }

    pub fn project_dir(&self, cwd: &Path) -> PathBuf {
        cwd.join(".coding-brain")
    }
}

fn resolve_base(
    explicit: Option<&Path>,
    home: Option<&Path>,
    variable: &'static str,
    fallback: &str,
) -> Result<PathBuf, PathError> {
    if let Some(base) = explicit.filter(|base| !base.as_os_str().is_empty()) {
        if !base.is_absolute() {
            return Err(PathError::RelativeBase(variable));
        }
        return Ok(base.to_path_buf());
    }

    let home = home.ok_or(PathError::MissingHome)?;
    if !home.is_absolute() {
        return Err(PathError::RelativeBase("HOME"));
    }
    Ok(home.join(fallback))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(value: &str) -> Option<PathBuf> {
        Some(PathBuf::from(value))
    }

    #[test]
    fn resolves_xdg_paths_and_documented_fallbacks() {
        let explicit = PathEnvironment::new(path("/cfg"), path("/state"), path("/home/alex"));
        let paths = CodingBrainPaths::resolve(&explicit).unwrap();
        assert_eq!(
            paths.config_file(),
            Path::new("/cfg/coding-brain/config.toml")
        );
        assert_eq!(paths.state_root(), Path::new("/state/coding-brain"));

        let fallback =
            CodingBrainPaths::resolve(&PathEnvironment::new(None, None, path("/home/alex")))
                .unwrap();
        assert_eq!(
            fallback.config_file(),
            Path::new("/home/alex/.config/coding-brain/config.toml")
        );
        assert_eq!(
            fallback.state_root(),
            Path::new("/home/alex/.local/state/coding-brain")
        );
    }

    #[test]
    fn resolves_only_documented_project_paths() {
        let paths =
            CodingBrainPaths::resolve(&PathEnvironment::new(None, None, path("/home/alex")))
                .unwrap();
        assert_eq!(
            paths.project_config(Path::new("/work/repo")),
            Path::new("/work/repo/.coding-brain.toml")
        );
        assert_eq!(
            paths.project_dir(Path::new("/work/repo")),
            Path::new("/work/repo/.coding-brain")
        );
    }

    #[test]
    fn rejects_relative_or_unresolvable_bases() {
        assert_eq!(
            CodingBrainPaths::resolve(&PathEnvironment::new(path("cfg"), None, path("/home"))),
            Err(PathError::RelativeBase("XDG_CONFIG_HOME"))
        );
        assert_eq!(
            CodingBrainPaths::resolve(&PathEnvironment::new(None, None, None)),
            Err(PathError::MissingHome)
        );
    }

    #[test]
    fn empty_xdg_values_use_home_fallbacks() {
        let paths = CodingBrainPaths::resolve(&PathEnvironment::new(
            path(""),
            path(""),
            path("/home/alex"),
        ))
        .unwrap();
        assert_eq!(
            paths.config_file(),
            Path::new("/home/alex/.config/coding-brain/config.toml")
        );
        assert_eq!(
            paths.state_root(),
            Path::new("/home/alex/.local/state/coding-brain")
        );
    }
}
