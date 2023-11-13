use std::{io, fs};
use std::convert::TryInto;
use std::io::Write;
use std::path::{Path, PathBuf};
use codegen::{LazyCreateBuilder};
use debcrafter::{Map, Set};
use debcrafter::im_repr::{Package, PackageInstance, ServiceInstance};
use debcrafter::types::{VPackageName, Variant};
use debcrafter::error_report::Report;
use serde_derive::Deserialize;
use std::borrow::Borrow;
use either::Either;
use std::convert::TryFrom;

mod generator;
mod codegen;

#[derive(Deserialize)]
pub struct Repository {
    pub maintainer: String,
    pub sources: Map<String, Source>,
}

#[derive(Deserialize)]
pub struct Source {
    pub section: String,
    #[serde(default)]
    pub build_depends: Vec<String>,
    #[serde(default, rename = "with")]
    pub with_components: Set<String>,
    #[serde(default)]
    pub buildsystem: Option<String>,
    #[serde(default)]
    pub autoconf_params: Vec<String>,
    #[serde(default)]
    pub variants: Set<Variant>,
    pub packages: Set<VPackageName>,
    #[serde(default)]
    pub skip_debug_symbols: bool,
    #[serde(default)]
    pub skip_strip: bool,
}

#[derive(Deserialize)]
pub struct SingleSource {
    pub name: String,
    pub maintainer: Option<String>,
    #[serde(flatten)]
    pub source: Source,
}

struct ServiceRule {
    unit_name: String,
    refuse_manual_start: bool,
    refuse_manual_stop: bool,
}

static FILE_GENERATORS: &[(&str, fn(&PackageInstance, LazyCreateBuilder) -> io::Result<()>)] = &[
    ("config", crate::generator::config::generate),
    ("install", crate::generator::install::generate),
    ("dirs", crate::generator::dirs::generate),
    ("links", crate::generator::links::generate),
    ("manpages", crate::generator::manpages::generate),
    ("preinst", crate::generator::preinst::generate),
    ("postinst", crate::generator::postinst::generate),
    ("prerm", crate::generator::prerm::generate),
    ("postrm", crate::generator::postrm::generate),
    ("templates", crate::generator::templates::generate),
    ("triggers", crate::generator::triggers::generate),
];

fn gen_rules<I>(deb_dir: &Path, source: &Source, systemd_services: I) -> io::Result<()> where I: IntoIterator, <I as IntoIterator>::IntoIter: ExactSizeIterator, <I as IntoIterator>::Item: Borrow<ServiceRule> {
    let systemd_services = systemd_services.into_iter();
    let mut out = fs::File::create(deb_dir.join("rules")).expect("Failed to create control file");

    writeln!(out, "#!/usr/bin/make -f")?;
    writeln!(out)?;
    writeln!(out, "%:")?;
    write!(out, "\tdh $@")?;
    for component in &source.with_components {
        write!(out, " --with {}", component)?;
    }
    if let Some(buildsystem) = &source.buildsystem {
        write!(out, " --buildsystem {}", buildsystem)?;
    }
    writeln!(out)?;

    if systemd_services.len() > 0 {
        writeln!(out)?;
        writeln!(out, "override_dh_installsystemd:")?;
        for service in systemd_services {
            let service = service.borrow();

            write!(out, "\tdh_installsystemd --name={}", service.unit_name)?;
            if service.refuse_manual_start {
                write!(out, " --no-start")?;
            }
            if service.refuse_manual_stop {
                write!(out, " --no-stop-on-upgrade --no-restart-after-upgrade")?;
            }
            writeln!(out)?;
        }
    }
    if !source.autoconf_params.is_empty() {
        writeln!(out)?;
        writeln!(out, "override_dh_auto_configure:")?;
        write!(out, "\tdh_auto_configure --")?;
        for param in &source.autoconf_params {
            write!(out, " {}", param)?;
        }
        writeln!(out)?;
    }
    if source.skip_debug_symbols {
        writeln!(out)?;
        writeln!(out, "override_dh_dwz:")?;
    }
    if source.skip_strip {
        writeln!(out)?;
        writeln!(out, "override_dh_strip:")?;
    }
    Ok(())
}

fn gen_control(deb_dir: &Path, name: &str, source: &Source, maintainer: &str, needs_dh_systemd: bool) -> io::Result<()> {
    let mut out = fs::File::create(deb_dir.join("control")).expect("Failed to create control file");

    writeln!(out, "Source: {}", name)?;
    writeln!(out, "Section: {}", source.section)?;
    writeln!(out, "Priority: optional")?;
    writeln!(out, "Maintainer: {}", maintainer)?;
    write!(out, "Build-Depends: debhelper (>= 9)")?;
    if needs_dh_systemd {
        write!(out, ",\n               debhelper (>= 12.1.1)")?;
    }
    for build_dep in &source.build_depends {
        write!(out, ",\n               {}", build_dep)?;
    }
    writeln!(out)
}

fn copy_changelog(deb_dir: &Path, source: &Path) {
    let dest = deb_dir.join("changelog");

    match fs::copy(&source, &dest) {
        Ok(_) => (),
        Err(ref err) if err.kind() == std::io::ErrorKind::NotFound => (),
        Err(err) => panic!("Failed to copy changelog of from {} to {}: {}", source.display(), dest.display(), err),
    }
}

fn load_package(source_dir: &Path, package: &VPackageName) -> (Package, PathBuf, String) {
    let filename = package.sps_path(source_dir);
    let source = std::fs::read_to_string(&filename).unwrap_or_else(|error| panic!("failed to read {}: {}", filename.display(), error));
    let package = toml::from_str::<debcrafter::input::Package>(&source)
        .expect("Failed to parse package")
        .try_into()
        .unwrap_or_else(|error: debcrafter::im_repr::PackageError| error.report(filename.display().to_string(), &source));
    (package, filename, source)
}

fn create_lazy_builder(dest_dir: &Path, name: &str, extension: &str, append: bool) -> LazyCreateBuilder {
    let mut file_name = dest_dir.join(name);
    file_name.set_extension(extension);
    LazyCreateBuilder::new(file_name, append)
}

fn changelog_parse_version(changelog_path: &Path) -> String {
    let output = std::process::Command::new("dpkg-parsechangelog")
        .arg("-l")
        .arg(changelog_path)
        .args(&["-S", "Version"])
        .output()
        .expect("dpkg-parsechangelog failed");
    if !output.status.success() {
        panic!("dpkg-parsechangelog failed with status {}", output.status);
    }

    let mut version = String::from_utf8(output.stdout).expect("dpkg-parsechangelog output is not UTF-8");
    if version.ends_with('\n') {
        version.pop();
    }

    version
}

fn get_upstream_version(version: &str) -> &str {
    version.rfind('-').map(|pos| &version[..pos]).unwrap_or(version)
}

fn gen_source(dest: &Path, source_dir: &Path, name: &str, source: &mut Source, maintainer: &str, mut dep_file: Option<&mut fs::File>) {
    use debcrafter::im_repr::PackageError;

    let mut changelog_path = source_dir.join(name);
    changelog_path.set_extension("changelog");
    let version = changelog_parse_version(&changelog_path);
    let upstream_version = get_upstream_version(&version);
    let dir = dest.join(format!("{}-{}", name, upstream_version));
    let deb_dir = dir.join("debian");
    fs::create_dir_all(&deb_dir).expect("Failed to create debian directory");
    copy_changelog(&deb_dir, &changelog_path);

    let mut deps = Set::new();
    let mut deps_opt = dep_file.as_mut().map(|_| { &mut deps });

    // TODO: calculate debhelper dep instead
    gen_control(&deb_dir, name, source, maintainer, true).expect("Failed to generate control");
    std::fs::write(deb_dir.join("compat"), "12\n").expect("Failed to write debian/compat");

    let mut services = Vec::new();

    let packages = source.packages
        .iter()
        .map(|package| load_package(source_dir, &package));

    for (package, filename, package_source) in packages {
        use debcrafter::im_repr::PackageOps;
        let deps_opt = deps_opt.as_mut().map(|deps| { deps.insert(filename.clone()); &mut **deps});
        let includes = package
            .load_includes(source_dir, deps_opt)
            .into_iter()
            .map(|(name, package)| Ok((name.clone(), debcrafter::im_repr::Package::try_from(package).unwrap_or_else(|error| panic!("invalid package {:?}: {:?}", name, error)))))
            .collect::<Result<_, PackageError>>().expect("invalid package");

        let instances = if source.variants.is_empty() || !package.name.is_templated() {
            let instance = package.instantiate(None, Some(&includes));
            instance
                .validate()
                .unwrap_or_else(|error| error.report(filename.display().to_string(), package_source));
            Either::Left(std::iter::once(instance))
        } else {
            Either::Right(source.variants.iter()
                          .map(|variant| {
                              let instance = package.instantiate(Some(variant), Some(&includes));
                              instance
                                  .validate()
                                  .unwrap_or_else(|error| error.report(filename.display().to_string(), &package_source));
                              instance
                          }))
        };

        services.extend(instances
            .into_iter()
            .filter_map(|instance| {
                for &(extension, generator) in FILE_GENERATORS {
                    let out = create_lazy_builder(&deb_dir, &instance.name, extension, false);
                    generator(&instance, out).expect("Failed to generate file");
                }

                if let Some(service_name) = instance.service_name() {
                    let out = create_lazy_builder(&deb_dir, &instance.name, &format!("{}.service", service_name), false);
                    crate::generator::service::generate(&instance, out).expect("Failed to generate file");
                }

                let out = create_lazy_builder(&deb_dir, "control", "", true);
                generator::control::generate(&instance, out, &upstream_version, source.buildsystem.as_ref().map(AsRef::as_ref)).expect("Failed to generate file");
                generator::static_files::generate(&instance, &dir).expect("Failed to generate static files");

                instance.as_service().map(|service| ServiceRule {
                    unit_name: ServiceInstance::service_name(&service).to_owned(),
                    refuse_manual_start: service.spec.refuse_manual_start,
                    refuse_manual_stop: service.spec.refuse_manual_stop,
                })
            }));
    }

    if let Some(dep_file) = dep_file {
        (|| -> Result<(), io::Error> {
            write!(dep_file, "{}/debcrafter-{}.stamp:", dest.display(), name)?;
            for dep in &deps {
                write!(dep_file, " {}", dep.display())?;
            }
            writeln!(dep_file, "\n")?;
            Ok(())
        })().expect("Failed to write into dependency file")
    }

    gen_rules(&deb_dir, source, &services).expect("Failed to generate rules");
}

fn check(source_dir: &Path, name: &str, source: &mut Source) {
    use debcrafter::im_repr::PackageError;

    let mut changelog_path = source_dir.join(name);
    changelog_path.set_extension("changelog");
    let version = changelog_parse_version(&changelog_path);
    get_upstream_version(&version);

    let packages = source.packages
        .iter()
        .map(|package| load_package(source_dir, &package));

    for (package, filename, package_source) in packages {
        let package = debcrafter::im_repr::Package::try_from(package).expect("invalid package");
        let includes = package
            .load_includes(source_dir, None)
            .into_iter()
            .map(|(name, package)| Ok((name, debcrafter::im_repr::Package::try_from(package).expect("invalid package"))))
            .collect::<Result<_, PackageError>>().expect("invalid package");

        if source.variants.is_empty() || !package.name.is_templated() {
            let instance = package.instantiate(None, Some(&includes));
            instance
                .validate()
                .unwrap_or_else(|error| error.report(filename.display().to_string(), package_source));
        } else {
            for variant in &source.variants {
                let instance = package.instantiate(Some(&variant), Some(&includes));
                instance
                    .validate()
                    .unwrap_or_else(|error| error.report(filename.display().to_string(), &package_source));
            }
        };
    }
}

fn main() {
    let mut args = std::env::args_os();
    args.next().expect("Not even zeroth argument given");
    let spec_file = std::path::PathBuf::from(args.next().expect("Source not specified."));
    let dest = std::path::PathBuf::from(args.next().expect("Dest not specified."));
    let mut split_source = false;
    let mut write_deps = None;
    let mut check_only = false;

    while let Some(arg) = args.next() {
        if arg == "--split-source" {
            split_source = true;
        }

        if arg == "--write-deps" {
            let file = args.next().expect("missing argument for --write-deps");
            write_deps = Some(file.into_string().expect("Invalid UTF econding"));
        }

        if arg == "--check" {
            check_only = true;
        }
    }

    if write_deps.is_some() && check_only {
        panic!("Specifying --check and --write-deps at the same time doesn't make sense");
    }

    let mut dep_file = write_deps.map(|dep_file| fs::File::create(dep_file).expect("failed to open dependency file"));

    if split_source {
        let mut source = debcrafter::input::load_toml::<SingleSource, _>(&spec_file).expect("Failed to load source");
        if check_only {
            check(spec_file.parent().unwrap_or(".".as_ref()), &source.name, &mut source.source);
        } else {
            let maintainer = source.maintainer.or_else(|| std::env::var("DEBEMAIL").ok()).expect("missing maintainer");

            gen_source(&dest, spec_file.parent().unwrap_or(".".as_ref()), &source.name, &mut source.source, &maintainer, dep_file.as_mut())
        }
    } else {
        let repo = debcrafter::input::load_toml::<Repository, _>(&spec_file).expect("Failed to load repository");
        
        for (name, mut source) in repo.sources {
            if check_only {
                check(spec_file.parent().unwrap_or(".".as_ref()), &name, &mut source);
            } else {
                gen_source(&dest, spec_file.parent().unwrap_or(".".as_ref()), &name, &mut source, &repo.maintainer, dep_file.as_mut())
            }
        }
    }
}
