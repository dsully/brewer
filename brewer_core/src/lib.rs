use std::collections::HashSet;
use std::fmt::Debug;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;

use anyhow::anyhow;
use derive_builder::Builder;
use log::info;
use serde::Deserialize;

use crate::models::*;

pub mod models;

const DEFAULT_BREW_PATH: &str = "brew";

const BREW_PREFIX_ENV_KEY: &str = "HOMEBREW_PREFIX";

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const DEFAULT_BREW_PREFIX: &str = "/opt/homebrew";

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const DEFAULT_BREW_PREFIX: &str = "/usr/local";

#[cfg(target_os = "linux")]
const DEFAULT_BREW_PREFIX: &str = "/home/linuxbrew/.linuxbrew";

const BREW_BIN_REGISTRY_URL: &str =
    "https://raw.githubusercontent.com/Homebrew/homebrew-command-not-found/master/executables.txt";

const BREW_ANALYTICS_URL: &str = "https://formulae.brew.sh/api/analytics/install/30d.json";

#[derive(Builder, Clone)]
pub struct Brew {
    pub path: PathBuf,
    pub prefix: PathBuf,
}

impl Default for Brew {
    fn default() -> Self {
        let prefix_env = std::env::var(BREW_PREFIX_ENV_KEY).unwrap_or_default();

        let prefix = if prefix_env.is_empty() {
            DEFAULT_BREW_PREFIX.into()
        } else {
            prefix_env
        };

        Brew {
            path: DEFAULT_BREW_PATH.into(),
            prefix: prefix.into(),
        }
    }
}

impl Brew {
    const JSON_FLAG: &'static str = "--json=v2";

    fn brew(&self) -> Command {
        let mut command = Command::new(self.path.clone());

        command.env("HOMEBREW_NO_AUTO_UPDATE", "1");
        command.env("HOMEBREW_NO_ENV_HINTS", "1");

        command
    }

    pub fn install(&self, kegs: Vec<Keg>) -> anyhow::Result<()> {
        let (formulae, casks) = split_kegs(kegs);

        if !formulae.is_empty() {
            let status = self
                .brew()
                .arg("install")
                .arg("--formulae")
                .args(formulae.into_iter().map(|f| f.base.name))
                .status()?;

            if !status.success() {
                return Err(anyhow!("failed to install formulae"));
            }
        }

        if !casks.is_empty() {
            let status = self
                .brew()
                .arg("install")
                .arg("--casks")
                .args(casks.into_iter().map(|c| c.base.token))
                .status()?;

            if !status.success() {
                return Err(anyhow!("failed to install casks"));
            }
        }

        Ok(())
    }

    pub fn uninstall(&self, kegs: Vec<Keg>) -> anyhow::Result<()> {
        let (formulae, casks) = split_kegs(kegs);

        if !formulae.is_empty() {
            let status = self
                .brew()
                .arg("uninstall")
                .arg("--formulae")
                .args(formulae.into_iter().map(|f| f.base.name))
                .status()?;

            if !status.success() {
                return Err(anyhow!("failed to uninstall formulae"));
            }
        }

        if !casks.is_empty() {
            let status = self
                .brew()
                .arg("uninstall")
                .arg("--casks")
                .args(casks.into_iter().map(|c| c.base.token))
                .status()?;

            if !status.success() {
                return Err(anyhow!("failed to uninstall casks"));
            }
        }

        Ok(())
    }

    pub fn analytics(&self) -> anyhow::Result<formula::analytics::Store> {
        let body = reqwest::blocking::get(BREW_ANALYTICS_URL)?.bytes()?;

        #[derive(Deserialize)]
        struct Result {
            pub items: Vec<formula::analytics::Formula>,
        }

        let result: Result = serde_json::from_slice(body.iter().as_slice())?;

        let mut store = formula::analytics::Store::new();

        for item in result.items {
            store.insert(item.formula.clone(), item);
        }

        Ok(store)
    }

    pub fn executables(&self) -> anyhow::Result<formula::Executables> {
        let body = reqwest::blocking::get(BREW_BIN_REGISTRY_URL)?.text()?;
        let mut store = formula::Executables::new();

        for line in body.lines().filter(|l| !l.is_empty()) {
            let Some((lhs, rhs)) = line.split_once(':') else {
                continue;
            };

            let Some(index) = lhs.find('(') else {
                continue;
            };

            let name = &lhs[..index];
            let executables: HashSet<String> =
                rhs.split_whitespace().map(|s| s.to_string()).collect();

            store.insert(name.to_string(), executables);
        }

        Ok(store)
    }

    pub fn state(&self) -> anyhow::Result<State<formula::State, cask::State>> {
        let executables = self.executables()?;
        let analytics = self.analytics()?;
        let all = self.eval_all()?;

        let all: State<formula::Store, cask::Store> = State {
            formulae: all
                .formulae
                .into_iter()
                .map(|(name, base)| {
                    let executables = if let Some(e) = executables.get(&name) {
                        e.clone()
                    } else {
                        HashSet::new()
                    };

                    let analytics = if let Some(a) = analytics.get(&name) {
                        Some(a.clone())
                    } else {
                        analytics
                            .get(format!("{}/{}", base.tap, base.name).as_str())
                            .cloned()
                    };

                    (
                        name,
                        formula::Formula {
                            base,
                            executables,
                            analytics,
                        },
                    )
                })
                .collect(),
            casks: all
                .casks
                .into_iter()
                .map(|(name, base)| (name, cask::Cask { base }))
                .collect(),
        };

        let installed = self.installed(&all)?;

        Ok(State {
            formulae: formula::State {
                all: all.formulae,
                installed: installed.formulae,
            },
            casks: cask::State {
                all: all.casks,
                installed: installed.casks,
            },
        })
    }

    pub fn installed(
        &self,
        all: &State<formula::Store, cask::Store>,
    ) -> anyhow::Result<State<formula::installed::Store, cask::installed::Store>> {
        let formulae = self.eval_installed_formulae(&all.formulae)?;
        let casks = self.eval_installed_casks(&all.casks)?;

        Ok(State { formulae, casks })
    }

    fn eval_installed_casks(&self, store: &cask::Store) -> anyhow::Result<cask::installed::Store> {
        let mut installed = cask::installed::Store::new();

        for (name, versions) in self.eval_installed_casks_versions()? {
            let Some(cask) = store.get(&name) else {
                continue;
            };

            installed.insert(
                name,
                cask::installed::Cask {
                    upstream: cask.clone(),
                    versions,
                },
            );
        }

        Ok(installed)
    }

    fn eval_installed_casks_versions(&self) -> anyhow::Result<cask::installed::VersionsStore> {
        let caskroom = self.prefix.join("Caskroom").read_dir()?;

        let mut store = cask::installed::VersionsStore::new();

        for entry in caskroom {
            let entry = entry?;
            let path = entry.path();

            let Some(name) = path.file_name() else {
                continue;
            };

            let name = name.to_string_lossy().to_string();
            let mut versions: HashSet<String> = HashSet::new();

            for entry in path.canonicalize()?.read_dir()? {
                let entry = entry?;
                let path = entry.path();

                let Some(name) = path.file_name() else {
                    continue;
                };

                let name = name.to_string_lossy().to_string();

                if Self::is_dotfile(&name) {
                    continue;
                }

                versions.insert(name);
            }

            store.insert(name, versions);
        }

        Ok(store)
    }

    fn eval_installed_formulae(
        &self,
        store: &formula::Store,
    ) -> anyhow::Result<formula::installed::Store> {
        let mut installed = formula::installed::Store::new();

        for (name, receipt) in self.eval_installed_formulae_receipts()? {
            let Some(formula) = store.get(&name) else {
                continue;
            };

            installed.insert(
                name,
                formula::installed::Formula {
                    upstream: formula.clone(),
                    receipt,
                },
            );
        }

        Ok(installed)
    }

    fn eval_installed_formulae_receipts(&self) -> anyhow::Result<formula::receipt::Store> {
        let opt = self.prefix.join("opt").read_dir()?;

        let mut store = formula::receipt::Store::new();

        for entry in opt {
            let entry = entry?;
            let path = entry.path();

            let Some(name) = path.file_name() else {
                continue;
            };

            let name = name.to_string_lossy().to_string();

            if Self::is_dotfile(&name) {
                continue;
            }

            let receipt_path = path.canonicalize()?.join("INSTALL_RECEIPT.json");

            let mut file = File::open(receipt_path)?;
            let mut data = Vec::new();

            file.read_to_end(&mut data)?;

            let receipt: formula::receipt::Receipt = serde_json::from_slice(data.as_slice())?;

            store.insert(name.clone(), receipt);
        }

        Ok(store)
    }

    fn is_dotfile(name: &str) -> bool {
        name.starts_with('.')
    }

    fn eval_all(&self) -> anyhow::Result<State<formula::base::Store, cask::base::Store>> {
        let mut command = self.brew();

        let command = command.arg("info").arg("--eval-all").arg(Self::JSON_FLAG);

        info!("running {:?}", command);

        let output = command.output()?;

        #[derive(Deserialize)]
        struct Result {
            formulae: Vec<formula::base::Formula>,
            casks: Vec<cask::base::Cask>,
        }

        let result: Result = serde_json::from_slice(output.stdout.as_slice())?;

        let formulae: formula::base::Store = result
            .formulae
            .into_iter()
            .map(|f| (f.name.clone(), f))
            .collect();

        let casks: cask::base::Store = result
            .casks
            .into_iter()
            .map(|c| (c.token.clone(), c))
            .collect();

        Ok(State { formulae, casks })
    }
}

fn split_kegs(kegs: Vec<Keg>) -> (Vec<formula::Formula>, Vec<cask::Cask>) {
    let mut formulae: Vec<formula::Formula> = Vec::with_capacity(kegs.len());
    let mut casks: Vec<cask::Cask> = Vec::with_capacity(kegs.len());

    for keg in kegs {
        match keg {
            Keg::Formula(formula) => formulae.push(formula),
            Keg::Cask(cask) => casks.push(cask),
        };
    }

    (formulae, casks)
}
