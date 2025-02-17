#![doc = include_str!("../README.md")]

mod ext;

use anyhow::{bail, Context, Result as AResult};
use cargo_metadata::{camino::Utf8PathBuf, Target};
use clap::Parser;
use dialoguer::{Confirm, Select};

use std::{
    borrow::Cow,
    ffi::OsString,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Command, Stdio},
    str::FromStr,
};

use self::ext::Ext;
use ext_php_rs::describe::ToStub;

/// Generates mock symbols required to generate stub files from a downstream
/// crates CLI application.
#[macro_export]
macro_rules! stub_symbols {
    ($($s: ident),*) => {
        $(
            $crate::stub_symbols!(@INTERNAL; $s);
        )*
    };
    (@INTERNAL; $s: ident) => {
        #[allow(non_upper_case_globals)]
        #[no_mangle]
        pub static mut $s: *mut () = ::std::ptr::null_mut();
    };
}

/// Result type returned from the [`run`] function.
pub type Result = anyhow::Result<()>;

/// Runs the CLI application. Returns nothing in a result on success.
pub fn run() -> Result {
    let mut args: Vec<_> = std::env::args().collect();

    // When called as a cargo subcommand, the second argument given will be the
    // subcommand, in this case `php`. We don't want this so we remove from args and
    // pass it to clap.
    if args.get(1).map(|nth| nth == "php").unwrap_or(false) {
        args.remove(1);
    }

    Args::parse_from(args).handle()
}

#[derive(Parser)]
#[clap(
    about = "Installs extensions and generates stub files for PHP extensions generated with `ext-php-rs`.",
    author = "David Cole <david.cole1340@gmail.com>",
    version = env!("CARGO_PKG_VERSION")
)]
enum Args {
    /// Installs the extension in the current PHP installation.
    ///
    /// This copies the extension to the PHP installation and adds the
    /// extension to a PHP configuration file.
    ///
    /// Note that this uses the `php-config` executable installed alongside PHP
    /// to locate your `php.ini` file and extension directory. If you want to
    /// use a different `php-config`, the application will read the `PHP_CONFIG`
    /// variable (if it is set), and will use this as the path to the executable
    /// instead.
    Install(Install),
    /// Removes the extension in the current PHP installation.
    ///
    /// This deletes the extension from the PHP installation and also removes it
    /// from the main PHP configuration file.
    ///
    /// Note that this uses the `php-config` executable installed alongside PHP
    /// to locate your `php.ini` file and extension directory. If you want to
    /// use a different `php-config`, the application will read the `PHP_CONFIG`
    /// variable (if it is set), and will use this as the path to the executable
    /// instead.
    Remove(Remove),
    /// Generates stub PHP files for the extension.
    ///
    /// These stub files can be used in IDEs to provide typehinting for
    /// extension classes, functions and constants.
    Stubs(Stubs),
}

#[derive(Parser)]
struct Install {
    /// Changes the path that the extension is copied to. This will not
    /// activate the extension unless `ini_path` is also passed.
    #[clap(long)]
    install_dir: Option<PathBuf>,
    /// Path to the `php.ini` file to update with the new extension.
    #[clap(long)]
    ini_path: Option<PathBuf>,
    /// Installs the extension but doesn't enable the extension in the `php.ini`
    /// file.
    #[clap(long)]
    disable: bool,
    /// Whether to install the release version of the extension.
    #[clap(long)]
    release: bool,
    /// Path to the Cargo manifest of the extension. Defaults to the manifest in
    /// the directory the command is called.
    #[clap(long)]
    manifest: Option<PathBuf>,
}

#[derive(Parser)]
struct Remove {
    /// Changes the path that the extension will be removed from. This will not
    /// remove the extension from a configuration file unless `ini_path` is also
    /// passed.
    #[clap(long)]
    install_dir: Option<PathBuf>,
    /// Path to the `php.ini` file to remove the extension from.
    #[clap(long)]
    ini_path: Option<PathBuf>,
    /// Path to the Cargo manifest of the extension. Defaults to the manifest in
    /// the directory the command is called.
    #[clap(long)]
    manifest: Option<PathBuf>,
}

#[derive(Parser)]
struct Stubs {
    /// Path to extension to generate stubs for. Defaults for searching the
    /// directory the executable is located in.
    ext: Option<PathBuf>,
    /// Path used to store generated stub file. Defaults to writing to
    /// `<ext-name>.stubs.php` in the current directory.
    #[clap(short, long)]
    out: Option<PathBuf>,
    /// Print stubs to stdout rather than write to file. Cannot be used with
    /// `out`.
    #[clap(long, conflicts_with = "out")]
    stdout: bool,
    /// Path to the Cargo manifest of the extension. Defaults to the manifest in
    /// the directory the command is called.
    ///
    /// This cannot be provided alongside the `ext` option, as that option
    /// provides a direct path to the extension shared library.
    #[clap(long, conflicts_with = "ext")]
    manifest: Option<PathBuf>,
}

impl Args {
    pub fn handle(self) -> Result {
        match self {
            Args::Install(install) => install.handle(),
            Args::Remove(remove) => remove.handle(),
            Args::Stubs(stubs) => stubs.handle(),
        }
    }
}

impl Install {
    pub fn handle(self) -> Result {
        let artifact = find_ext(&self.manifest)?;
        let ext_path = build_ext(&artifact, self.release)?;

        let (mut ext_dir, mut php_ini) = if let Some(install_dir) = self.install_dir {
            (install_dir, None)
        } else {
            let php_config = PhpConfig::new();
            (php_config.get_ext_dir()?, Some(php_config.get_php_ini()?))
        };

        if let Some(ini_path) = self.ini_path {
            php_ini = Some(ini_path);
        }

        if !Confirm::new()
            .with_prompt(format!(
                "Are you sure you want to install the extension `{}`?",
                artifact.name
            ))
            .interact()?
        {
            bail!("Installation cancelled.");
        }

        debug_assert!(ext_path.is_file());
        let ext_name = ext_path.file_name().expect("ext path wasn't a filepath");

        if ext_dir.is_dir() {
            ext_dir.push(ext_name);
        }

        std::fs::copy(&ext_path, &ext_dir).with_context(|| {
            "Failed to copy extension from target directory to extension directory"
        })?;

        if let Some(php_ini) = php_ini {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(php_ini)
                .with_context(|| "Failed to open `php.ini`")?;

            let mut ext_line = format!("extension={}", ext_name);

            let mut new_lines = vec![];
            for line in BufReader::new(&file).lines() {
                let line = line.with_context(|| "Failed to read line from `php.ini`")?;
                if !line.contains(&ext_line) {
                    new_lines.push(line);
                }
            }

            // Comment out extension if user specifies disable flag
            if self.disable {
                ext_line.insert(0, ';');
            }

            new_lines.push(ext_line);
            file.write(new_lines.join("\n").as_bytes())
                .with_context(|| "Failed to update `php.ini`")?;
        }

        Ok(())
    }
}

impl Remove {
    pub fn handle(self) -> Result {
        use std::env::consts;

        let artifact = find_ext(&self.manifest)?;

        let (mut ext_path, mut php_ini) = if let Some(install_dir) = self.install_dir {
            (install_dir, None)
        } else {
            let php_config = PhpConfig::new();
            (php_config.get_ext_dir()?, Some(php_config.get_php_ini()?))
        };

        if let Some(ini_path) = self.ini_path {
            php_ini = Some(ini_path);
        }

        let ext_file = format!(
            "{}{}{}",
            consts::DLL_PREFIX,
            artifact.name.replace("-", "_"),
            consts::DLL_SUFFIX
        );
        ext_path.push(&ext_file);

        if !ext_path.is_file() {
            bail!("Unable to find extension installed.");
        }

        if !Confirm::new()
            .with_prompt(format!(
                "Are you sure you want to remove the extension `{}`?",
                artifact.name
            ))
            .interact()?
        {
            bail!("Installation cancelled.");
        }

        std::fs::remove_file(ext_path).with_context(|| "Failed to remove extension")?;

        if let Some(php_ini) = php_ini.filter(|path| path.is_file()) {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(php_ini)
                .with_context(|| "Failed to open `php.ini`")?;

            let mut new_lines = vec![];
            for line in BufReader::new(&file).lines() {
                let line = line.with_context(|| "Failed to read line from `php.ini`")?;
                if !line.contains(&ext_file) {
                    new_lines.push(line);
                }
            }

            file.write(new_lines.join("\n").as_bytes())
                .with_context(|| "Failed to update `php.ini`")?;
        }

        Ok(())
    }
}

impl Stubs {
    pub fn handle(self) -> Result {
        let ext_path = if let Some(ext_path) = self.ext {
            ext_path
        } else {
            let target = find_ext(&self.manifest)?;
            build_ext(&target, false)?.into()
        };

        if !ext_path.is_file() {
            bail!("Invalid extension path given, not a file.");
        }

        let ext = Ext::load(ext_path)?;
        let result = ext.describe();

        // Ensure extension and CLI `ext-php-rs` versions are compatible.
        let cli_version = semver::VersionReq::from_str(ext_php_rs::VERSION).with_context(|| {
            "Failed to parse `ext-php-rs` version that `cargo php` was compiled with"
        })?;
        let ext_version = semver::Version::from_str(result.version).with_context(|| {
            "Failed to parse `ext-php-rs` version that your extension was compiled with"
        })?;

        if !cli_version.matches(&ext_version) {
            bail!("Extension was compiled with an incompatible version of `ext-php-rs` - Extension: {}, CLI: {}", ext_version, cli_version);
        }

        let stubs = result
            .module
            .to_stub()
            .with_context(|| "Failed to generate stubs.")?;

        if self.stdout {
            print!("{}", stubs);
        } else {
            let out_path = if let Some(out_path) = &self.out {
                Cow::Borrowed(out_path)
            } else {
                let mut cwd = std::env::current_dir()
                    .with_context(|| "Failed to get current working directory")?;
                cwd.push(format!("{}.stubs.php", result.module.name));
                Cow::Owned(cwd)
            };

            std::fs::write(out_path.as_ref(), &stubs)
                .with_context(|| "Failed to write stubs to file")?;
        }

        Ok(())
    }
}

struct PhpConfig {
    path: OsString,
}

impl PhpConfig {
    /// Creates a new `php-config` instance.
    pub fn new() -> Self {
        Self {
            path: if let Some(php_config) = std::env::var_os("PHP_CONFIG") {
                php_config
            } else {
                OsString::from("php-config")
            },
        }
    }

    /// Calls `php-config` and retrieves the extension directory.
    pub fn get_ext_dir(&self) -> AResult<PathBuf> {
        Ok(PathBuf::from(
            self.exec(
                |cmd| cmd.arg("--extension-dir"),
                "retrieve extension directory",
            )?
            .trim(),
        ))
    }

    /// Calls `php-config` and retrieves the `php.ini` file path.
    pub fn get_php_ini(&self) -> AResult<PathBuf> {
        let mut path = PathBuf::from(
            self.exec(|cmd| cmd.arg("--ini-path"), "retrieve `php.ini` path")?
                .trim(),
        );
        path.push("php.ini");

        if !path.exists() {
            File::create(&path).with_context(|| "Failed to create `php.ini`")?;
        }

        Ok(path)
    }

    /// Executes the `php-config` binary. The given function `f` is used to
    /// modify the given mutable [`Command`]. If successful, a [`String`]
    /// representing stdout is returned.
    fn exec<F>(&self, f: F, ctx: &str) -> AResult<String>
    where
        F: FnOnce(&mut Command) -> &mut Command,
    {
        let mut cmd = Command::new(&self.path);
        f(&mut cmd);
        let out = cmd
            .output()
            .with_context(|| format!("Failed to {} from `php-config`", ctx))?;
        String::from_utf8(out.stdout)
            .with_context(|| "Failed to convert `php-config` output to string")
    }
}

/// Attempts to find an extension in the target directory.
fn find_ext(manifest: &Option<PathBuf>) -> AResult<cargo_metadata::Target> {
    // TODO(david): Look for cargo manifest option or env
    let mut cmd = cargo_metadata::MetadataCommand::new();
    if let Some(manifest) = manifest {
        cmd.manifest_path(manifest);
    }

    let meta = cmd
        .features(cargo_metadata::CargoOpt::AllFeatures)
        .exec()
        .with_context(|| "Failed to call `cargo metadata`")?;

    let package = meta
        .root_package()
        .with_context(|| "Failed to retrieve metadata about crate")?;

    let targets: Vec<_> = package
        .targets
        .iter()
        .filter(|target| {
            target
                .crate_types
                .iter()
                .any(|ty| ty == "dylib" || ty == "cdylib")
        })
        .collect();

    let target = match targets.len() {
        0 => bail!("No library targets were found."),
        1 => targets[0],
        _ => {
            let target_names: Vec<_> = targets.iter().map(|target| &target.name).collect();
            let chosen = Select::new()
                .with_prompt("There were multiple library targets detected in the project. Which would you like to use?")
                .items(&target_names)
                .interact()?;
            targets[chosen]
        }
    };

    Ok(target.clone())
}

/// Compiles the extension, searching for the given target artifact. If found,
/// the path to the extension dynamic library is returned.
///
/// # Parameters
///
/// * `target` - The target to compile.
/// * `release` - Whether to compile the target in release mode.
///
/// # Returns
///
/// The path to the target artifact.
fn build_ext(target: &Target, release: bool) -> AResult<Utf8PathBuf> {
    let mut cmd = Command::new("cargo");
    cmd.arg("build")
        .arg("--message-format=json-render-diagnostics");
    if release {
        cmd.arg("--release");
    }

    let mut spawn = cmd
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| "Failed to spawn `cargo build`")?;
    let reader = BufReader::new(
        spawn
            .stdout
            .take()
            .with_context(|| "Failed to take `cargo build` stdout")?,
    );

    let mut artifact = None;
    for message in cargo_metadata::Message::parse_stream(reader) {
        let message = message.with_context(|| "Invalid message received from `cargo build`")?;
        match message {
            cargo_metadata::Message::CompilerArtifact(a) => {
                if &a.target == target {
                    artifact = Some(a);
                }
            }
            cargo_metadata::Message::BuildFinished(b) => {
                if !b.success {
                    bail!("Compilation failed, cancelling installation.")
                } else {
                    break;
                }
            }
            _ => continue,
        }
    }

    let artifact = artifact.with_context(|| "Extension artifact was not compiled")?;
    for file in artifact.filenames {
        if file.extension() == Some(std::env::consts::DLL_EXTENSION) {
            return Ok(file);
        }
    }

    bail!("Failed to retrieve extension path from artifact")
}
