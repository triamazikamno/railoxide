use std::path::PathBuf;

use directories::BaseDirs;
use structopt::StructOpt;

const DEFAULT_DB_PATH: &str = "db";
const APP_DATA_DIR: &str = "RailOxide";

#[derive(Clone, StructOpt)]
#[structopt(name = "wallet", about = "Railgun wallet desktop GUI.")]
pub(crate) struct Options {
    /// Dedicated profile directory. On Unix it (or a symlink's target) is made private (0700).
    #[structopt(long, parse(from_os_str))]
    pub(crate) db_path: Option<PathBuf>,
}

pub(crate) fn default_db_path() -> PathBuf {
    BaseDirs::new().map_or_else(
        || PathBuf::from(DEFAULT_DB_PATH),
        |dirs| dirs.data_local_dir().join(APP_DATA_DIR),
    )
}

impl Options {
    pub(crate) fn from_args() -> Self {
        <Self as StructOpt>::from_args()
    }
}

#[cfg(test)]
mod tests {
    use super::Options;
    use structopt::StructOpt;

    #[test]
    fn accepts_db_path() {
        let options =
            Options::from_iter_safe(["wallet", "--db-path", "custom-db"]).expect("parse db path");
        assert_eq!(
            options.db_path.as_deref(),
            Some(std::path::Path::new("custom-db"))
        );
    }

    #[test]
    fn rejects_removed_configuration_flags() {
        for flag in [
            "--network-mode",
            "--proxy",
            "--build-cache",
            "--poi-read-source",
            "--poi-artifact-gateway",
        ] {
            assert!(
                Options::from_iter_safe(["wallet", flag, "value"]).is_err(),
                "{flag} should be rejected"
            );
        }
    }
}
