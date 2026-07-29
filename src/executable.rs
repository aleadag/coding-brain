use std::path::Path;

pub(crate) const CURRENT_PROGRAM: &str = "cbrain";
pub(crate) const STALE_MANAGED_PROGRAMS: &[&str] = &["coding-brain", "codexctl"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProgramIdentity {
    Current,
    StaleManaged,
    Unmanaged,
}

pub(crate) fn classify_program(program: &str) -> ProgramIdentity {
    if program.ends_with(std::path::is_separator) {
        return ProgramIdentity::Unmanaged;
    }
    let path = Path::new(program);
    if !path.is_absolute() && path.components().count() != 1 {
        return ProgramIdentity::Unmanaged;
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return ProgramIdentity::Unmanaged;
    };
    if name == CURRENT_PROGRAM {
        ProgramIdentity::Current
    } else if STALE_MANAGED_PROGRAMS.contains(&name) {
        ProgramIdentity::StaleManaged
    } else {
        ProgramIdentity::Unmanaged
    }
}

pub(crate) fn is_current_program(program: &str) -> bool {
    classify_program(program) == ProgramIdentity::Current
}

pub(crate) fn is_managed_program(program: &str) -> bool {
    classify_program(program) != ProgramIdentity::Unmanaged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_only_exact_current_and_stale_basenames() {
        for program in ["cbrain", "/nix/store/hash/bin/cbrain"] {
            assert_eq!(classify_program(program), ProgramIdentity::Current);
        }
        for program in [
            "coding-brain",
            "codexctl",
            "/usr/local/bin/coding-brain",
            "/opt/tools/codexctl",
        ] {
            assert_eq!(classify_program(program), ProgramIdentity::StaleManaged);
        }
        for program in [
            "",
            "cbrain-old",
            "coding-brain-wrapper",
            "my-codexctl",
            "./cbrain",
            "bin/coding-brain",
            "tools/codexctl",
            "/usr/local/bin/",
            "cbrain/",
            "/nix/store/hash/bin/cbrain/",
            "coding-brain/",
            "/usr/local/bin/coding-brain/",
            "codexctl/",
            "/opt/tools/codexctl/",
        ] {
            assert_eq!(classify_program(program), ProgramIdentity::Unmanaged);
        }
    }
}
