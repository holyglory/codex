#[cfg(any(not(debug_assertions), test))]
use codex_install_context::InstallContext;
#[cfg(any(not(debug_assertions), test))]
use codex_install_context::InstallMethod;

/// Update action the CLI should perform after the TUI exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    /// Update via `npm install -g @holyglory/codex@latest`.
    NpmGlobalLatest,
    /// Update via `bun install -g @holyglory/codex@latest`.
    BunGlobalLatest,
    /// Update via `vp install -g @holyglory/codex@latest`.
    VitePlusGlobalLatest,
    /// Update via `pnpm add -g @holyglory/codex@latest`.
    PnpmGlobalLatest,
}

impl UpdateAction {
    #[cfg(any(not(debug_assertions), test))]
    pub(crate) fn from_install_context(context: &InstallContext) -> Option<Self> {
        match &context.method {
            InstallMethod::Npm => Some(UpdateAction::NpmGlobalLatest),
            InstallMethod::Bun => Some(UpdateAction::BunGlobalLatest),
            InstallMethod::VitePlus => Some(UpdateAction::VitePlusGlobalLatest),
            InstallMethod::Pnpm => Some(UpdateAction::PnpmGlobalLatest),
            // This fork has no Homebrew cask or public standalone updater.
            // Those installations must use their original delivery workflow.
            InstallMethod::Brew | InstallMethod::Standalone { .. } | InstallMethod::Other => None,
        }
    }

    /// Returns the list of command-line arguments for invoking the update.
    pub fn command_args(self) -> (&'static str, &'static [&'static str]) {
        match self {
            UpdateAction::NpmGlobalLatest => ("npm", &["install", "-g", "@holyglory/codex@latest"]),
            UpdateAction::BunGlobalLatest => ("bun", &["install", "-g", "@holyglory/codex@latest"]),
            UpdateAction::VitePlusGlobalLatest => {
                ("vp", &["install", "-g", "@holyglory/codex@latest"])
            }
            UpdateAction::PnpmGlobalLatest => ("pnpm", &["add", "-g", "@holyglory/codex@latest"]),
        }
    }

    /// Returns string representation of the command-line arguments for invoking the update.
    pub fn command_str(self) -> String {
        let (command, args) = self.command_args();
        shlex::try_join(std::iter::once(command).chain(args.iter().copied()))
            .unwrap_or_else(|_| format!("{command} {}", args.join(" ")))
    }
}

#[cfg(not(debug_assertions))]
pub fn get_update_action() -> Option<UpdateAction> {
    UpdateAction::from_install_context(InstallContext::current())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_install_context::StandalonePlatform;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;

    #[test]
    fn maps_install_context_to_update_action() {
        let native_release_dir =
            AbsolutePathBuf::from_absolute_path(std::env::temp_dir().join("native-release"))
                .expect("temp dir path should be absolute");

        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Other,
                package_layout: None,
            }),
            None
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Npm,
                package_layout: None,
            }),
            Some(UpdateAction::NpmGlobalLatest)
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Bun,
                package_layout: None,
            }),
            Some(UpdateAction::BunGlobalLatest)
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Pnpm,
                package_layout: None,
            }),
            Some(UpdateAction::PnpmGlobalLatest)
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Brew,
                package_layout: None,
            }),
            None
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Standalone {
                    platform: StandalonePlatform::Unix,
                    release_dir: native_release_dir.clone(),
                    resources_dir: Some(native_release_dir.join("codex-resources")),
                },
                package_layout: None,
            }),
            None
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Standalone {
                    platform: StandalonePlatform::Windows,
                    release_dir: native_release_dir.clone(),
                    resources_dir: Some(native_release_dir.join("codex-resources")),
                },
                package_layout: None,
            }),
            None
        );
    }

    #[test]
    fn package_manager_context_preserves_manager_and_updates_the_fork() {
        for (method, command, verb) in [
            (InstallMethod::Npm, "npm", "install"),
            (InstallMethod::Bun, "bun", "install"),
            (InstallMethod::VitePlus, "vp", "install"),
            (InstallMethod::Pnpm, "pnpm", "add"),
        ] {
            let action = UpdateAction::from_install_context(&InstallContext {
                method,
                package_layout: None,
            })
            .expect("package manager supports fork updates");
            assert_eq!(
                action.command_args(),
                (command, &[verb, "-g", "@holyglory/codex@latest"][..])
            );
            assert_eq!(
                shlex::split(&action.command_str()).expect("displayed command is executable"),
                vec![command, verb, "-g", "@holyglory/codex@latest"]
            );
        }
    }
}
