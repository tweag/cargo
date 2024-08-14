//! Interacts with the registry [publish API][1].
//!
//! [1]: https://doc.rust-lang.org/nightly/cargo/reference/registry-web-api.html#publish

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::io::Seek;
use std::io::SeekFrom;
use std::time::Duration;

use anyhow::bail;
use anyhow::Context as _;
use cargo_credential::Operation;
use cargo_credential::Secret;
use cargo_util::paths;
use crates_io::NewCrate;
use crates_io::NewCrateDependency;
use crates_io::Registry;
use itertools::Itertools;

use crate::core::dependency::DepKind;
use crate::core::manifest::ManifestMetadata;
use crate::core::resolver::CliFeatures;
use crate::core::Dependency;
use crate::core::Package;
use crate::core::PackageId;
use crate::core::PackageIdSpecQuery;
use crate::core::SourceId;
use crate::core::Workspace;
use crate::ops;
use crate::ops::PackageOpts;
use crate::ops::Packages;
use crate::ops::RegistryOrIndex;
use crate::sources::source::QueryKind;
use crate::sources::source::Source;
use crate::sources::SourceConfigMap;
use crate::sources::CRATES_IO_REGISTRY;
use crate::util::auth;
use crate::util::cache_lock::CacheLockMode;
use crate::util::context::JobsConfig;
use crate::util::toml::prepare_for_publish;
use crate::util::Graph;
use crate::util::Progress;
use crate::util::ProgressStyle;
use crate::CargoResult;
use crate::GlobalContext;

use super::super::check_dep_has_version;

pub struct PublishOpts<'gctx> {
    pub gctx: &'gctx GlobalContext,
    pub token: Option<Secret<String>>,
    pub reg_or_index: Option<RegistryOrIndex>,
    pub verify: bool,
    pub allow_dirty: bool,
    pub jobs: Option<JobsConfig>,
    pub keep_going: bool,
    pub to_publish: ops::Packages,
    pub targets: Vec<String>,
    pub dry_run: bool,
    pub cli_features: CliFeatures,
}

pub fn publish(ws: &Workspace<'_>, opts: &PublishOpts<'_>) -> CargoResult<()> {
    let multi_package_mode = ws.gctx().cli_unstable().package_workspace;
    let specs = opts.to_publish.to_package_id_specs(ws)?;

    if !multi_package_mode {
        if specs.len() > 1 {
            bail!("the `-p` argument must be specified to select a single package to publish")
        }
        if Packages::Default == opts.to_publish && ws.is_virtual() {
            bail!("the `-p` argument must be specified in the root of a virtual workspace")
        }
    }

    let member_ids: Vec<_> = ws.members().map(|p| p.package_id()).collect();
    // Check that the specs match members.
    for spec in &specs {
        spec.query(member_ids.clone())?;
    }
    let mut pkgs = ws.members_with_features(&specs, &opts.cli_features)?;
    // In `members_with_features_old`, it will add "current" package (determined by the cwd)
    // So we need filter
    pkgs = pkgs
        .into_iter()
        .filter(|(m, _)| specs.iter().any(|spec| spec.matches(m.package_id())))
        .collect();

    if multi_package_mode {
        publish_multi(ws, pkgs, opts)
    } else {
        // Double check. It is safe theoretically, unless logic has updated.
        assert_eq!(pkgs.len(), 1);
        let (pkg, cli_features) = pkgs.pop().unwrap();
        publish_one(ws, pkg, cli_features, opts)
    }
}

/// To be removed when package_workspace is stabilized.
fn publish_one(
    ws: &Workspace<'_>,
    pkg: &Package,
    cli_features: CliFeatures,
    opts: &PublishOpts<'_>,
) -> CargoResult<()> {
    let mut publish_registry = match opts.reg_or_index.as_ref() {
        Some(RegistryOrIndex::Registry(registry)) => Some(registry.clone()),
        _ => None,
    };
    if let Some(ref allowed_registries) = *pkg.publish() {
        if publish_registry.is_none() && allowed_registries.len() == 1 {
            // If there is only one allowed registry, push to that one directly,
            // even though there is no registry specified in the command.
            let default_registry = &allowed_registries[0];
            if default_registry != CRATES_IO_REGISTRY {
                // Don't change the registry for crates.io and don't warn the user.
                // crates.io will be defaulted even without this.
                opts.gctx.shell().note(&format!(
                    "found `{}` as only allowed registry. Publishing to it automatically.",
                    default_registry
                ))?;
                publish_registry = Some(default_registry.clone());
            }
        }

        let reg_name = publish_registry
            .clone()
            .unwrap_or_else(|| CRATES_IO_REGISTRY.to_string());
        if allowed_registries.is_empty() {
            bail!(
                "`{}` cannot be published.\n\
                 `package.publish` must be set to `true` or a non-empty list in Cargo.toml to publish.",
                pkg.name(),
            );
        } else if !allowed_registries.contains(&reg_name) {
            bail!(
                "`{}` cannot be published.\n\
                 The registry `{}` is not listed in the `package.publish` value in Cargo.toml.",
                pkg.name(),
                reg_name
            );
        }
    }
    // This is only used to confirm that we can create a token before we build the package.
    // This causes the credential provider to be called an extra time, but keeps the same order of errors.
    let ver = pkg.version().to_string();
    let operation = Operation::Read;

    let reg_or_index = match opts.reg_or_index.clone() {
        Some(RegistryOrIndex::Registry(_)) | None => {
            publish_registry.map(RegistryOrIndex::Registry)
        }
        val => val,
    };
    let source_ids = super::get_source_id(opts.gctx, reg_or_index.as_ref())?;
    let mut registry = super::registry(
        opts.gctx,
        &source_ids,
        opts.token.as_ref().map(Secret::as_deref),
        reg_or_index.as_ref(),
        true,
        Some(operation).filter(|_| !opts.dry_run),
    )?;
    verify_dependencies(pkg, &registry, source_ids.original)?;

    // Prepare a tarball, with a non-suppressible warning if metadata
    // is missing since this is being put online.
    let tarball = ops::package_one(
        ws,
        pkg,
        &PackageOpts {
            gctx: opts.gctx,
            verify: opts.verify,
            list: false,
            check_metadata: true,
            allow_dirty: opts.allow_dirty,
            to_package: Packages::Default,
            targets: opts.targets.clone(),
            jobs: opts.jobs.clone(),
            keep_going: opts.keep_going,
            cli_features,
            reg_or_index,
        },
    )?;

    if !opts.dry_run {
        let hash = cargo_util::Sha256::new()
            .update_file(tarball.file())?
            .finish_hex();
        let operation = Operation::Publish {
            name: pkg.name().as_str(),
            vers: &ver,
            cksum: &hash,
        };
        registry.set_token(Some(auth::auth_token(
            &opts.gctx,
            &source_ids.original,
            None,
            operation,
            vec![],
            false,
        )?));
    }

    opts.gctx
        .shell()
        .status("Uploading", pkg.package_id().to_string())?;
    transmit(
        opts.gctx,
        ws,
        pkg,
        tarball.file(),
        &mut registry,
        source_ids.original,
        opts.dry_run,
    )?;

    if !opts.dry_run {
        let short_pkg_description = format!("{} v{}", pkg.name(), pkg.version());
        let source_description = source_ids.original.to_string();
        ws.gctx().shell().status(
            "Uploaded",
            format!("{short_pkg_description} to {source_description}"),
        )?;

        const DEFAULT_TIMEOUT: u64 = 60;
        let timeout = if opts.gctx.cli_unstable().publish_timeout {
            let timeout: Option<u64> = opts.gctx.get("publish.timeout")?;
            timeout.unwrap_or(DEFAULT_TIMEOUT)
        } else {
            DEFAULT_TIMEOUT
        };
        if 0 < timeout {
            let timeout = Duration::from_secs(timeout);
            wait_for_publish(
                opts.gctx,
                source_ids.original,
                &HashSet::from([pkg.package_id()]),
                timeout,
            )?;
        }
    }

    Ok(())
}

fn publish_multi(
    ws: &Workspace<'_>,
    pkgs: Vec<(&Package, CliFeatures)>,
    opts: &PublishOpts<'_>,
) -> CargoResult<()> {
    let just_pkgs: Vec<_> = pkgs.iter().map(|p| p.0).collect();
    // FIXME: the package-vs-publish logic for validating registries should be almost the same,
    // but not *completely* (e.g. in that recent publish = false issue)
    let reg_or_index =
        ops::cargo_package::infer_registry(&just_pkgs, opts.reg_or_index.clone(), true)?;

    if let Some(RegistryOrIndex::Registry(ref registry)) = reg_or_index {
        if registry != CRATES_IO_REGISTRY {
            // Don't warn for crates.io.
            opts.gctx.shell().note(&format!(
                "found `{}` as only allowed registry. Publishing to it automatically.",
                registry
            ))?;
        }
    }

    // This is only used to confirm that we can create a token before we build the package.
    // This causes the credential provider to be called an extra time, but keeps the same order of errors.
    let source_ids = super::get_source_id(opts.gctx, reg_or_index.as_ref())?;
    let mut registry = super::registry(
        opts.gctx,
        &source_ids,
        opts.token.as_ref().map(Secret::as_deref),
        reg_or_index.as_ref(),
        true,
        Some(Operation::Read).filter(|_| !opts.dry_run),
    )?;

    let specs = Packages::from_flags(
        false,
        vec![],
        just_pkgs.iter().map(|p| p.name().to_string()).collect(),
    )?;
    let pkg_dep_graph = ops::cargo_package::package_with_dep_graph(
        ws,
        &PackageOpts {
            gctx: opts.gctx,
            verify: opts.verify,
            list: false,
            check_metadata: true,
            allow_dirty: opts.allow_dirty,
            to_package: specs,
            targets: opts.targets.clone(),
            jobs: opts.jobs.clone(),
            keep_going: opts.keep_going,
            cli_features: opts.cli_features.clone(),
            reg_or_index: reg_or_index.clone(),
        },
    )?;

    // Validate all the packages before publishing any of them.
    for (pkg, _) in pkg_dep_graph.packages.values() {
        verify_dependencies(pkg, &registry, source_ids.original)?;
    }

    let (mut order, mut ready) = PublishOrder::new(&pkg_dep_graph.graph);
    let mut to_confirm: HashSet<_> = ready.iter().copied().collect();
    let mut remaining: HashSet<_> = just_pkgs.iter().map(|pkg| pkg.package_id()).collect();

    while !to_confirm.is_empty() {
        if !ready.is_empty() {
            opts.gctx.shell().status(
                "Uploading",
                ready.iter().map(PackageId::to_string).sorted().join(", "),
            )?;
        }

        for pkg_id in &ready {
            let (pkg, (_features, tarball)) = &pkg_dep_graph.packages[pkg_id];

            if !opts.dry_run {
                let ver = pkg.version().to_string();

                tarball.file().seek(SeekFrom::Start(0))?;
                let hash = cargo_util::Sha256::new()
                    .update_file(tarball.file())?
                    .finish_hex();
                let operation = Operation::Publish {
                    name: pkg.name().as_str(),
                    vers: &ver,
                    cksum: &hash,
                };
                registry.set_token(Some(auth::auth_token(
                    &opts.gctx,
                    &source_ids.original,
                    None,
                    operation,
                    vec![],
                    false,
                )?));
            }

            transmit(
                opts.gctx,
                ws,
                pkg,
                tarball.file(),
                &mut registry,
                source_ids.original,
                opts.dry_run,
            )?;

            if !opts.dry_run {
                // Short does not include the registry name.
                let short_pkg_description = format!("{} v{}", pkg.name(), pkg.version());
                let source_description = source_ids.original.to_string();
                ws.gctx().shell().status(
                    "Uploaded",
                    format!("{short_pkg_description} to {source_description}"),
                )?;
            }
        }

        let finished = if opts.dry_run {
            ready.clone()
        } else {
            const DEFAULT_TIMEOUT: u64 = 60;
            let timeout = if opts.gctx.cli_unstable().publish_timeout {
                let timeout: Option<u64> = opts.gctx.get("publish.timeout")?;
                timeout.unwrap_or(DEFAULT_TIMEOUT)
            } else {
                DEFAULT_TIMEOUT
            };
            if 0 < timeout {
                let timeout = Duration::from_secs(timeout);
                wait_for_publish(opts.gctx, source_ids.original, &to_confirm, timeout)?
            } else {
                Vec::new()
            }
        };

        for id in &finished {
            to_confirm.remove(id);
            remaining.remove(id);
        }
        if finished.is_empty() {
            // If nothing finished, it means we timed out while waiting for confirmation.
            for id in &ready {
                remaining.remove(id);
            }

            if remaining.is_empty() {
                // It's ok that we timed out, because nothing was waiting on dependencies to
                // be confirmed.
                break;
            } else {
                let failed_list = package_list(remaining, "and");
                bail!("failed to publish {failed_list} because we timed out waiting for dependencies.");
            }
        }

        ready = order.mark_published(finished);
        to_confirm.extend(ready.iter().copied());
    }

    Ok(())
}

fn wait_for_publish(
    gctx: &GlobalContext,
    registry_src: SourceId,
    pkgs: &HashSet<PackageId>,
    timeout: Duration,
) -> CargoResult<Vec<PackageId>> {
    let mut source = SourceConfigMap::empty(gctx)?.load(registry_src, &HashSet::new())?;
    // Disable the source's built-in progress bars. Repeatedly showing a bunch
    // of independent progress bars can be a little confusing. There is an
    // overall progress bar managed here.
    source.set_quiet(true);
    let source_description = source.source_id().to_string();

    let now = std::time::Instant::now();
    let sleep_time = Duration::from_secs(1);
    let max = timeout.as_secs() as usize;
    // Short does not include the registry name.
    let short_pkg_descriptions = package_list(pkgs.iter().copied(), "or");
    gctx.shell().note(format!(
        "waiting for {short_pkg_descriptions} to be available at {source_description}.\n\
        You may press ctrl-c to skip waiting; the crate should be available shortly."
    ))?;
    let mut progress = Progress::with_style("Waiting", ProgressStyle::Ratio, gctx);
    progress.tick_now(0, max, "")?;
    let available = loop {
        {
            let _lock = gctx.acquire_package_cache_lock(CacheLockMode::DownloadExclusive)?;
            // Force re-fetching the source
            //
            // As pulling from a git source is expensive, we track when we've done it within the
            // process to only do it once, but we are one of the rare cases that needs to do it
            // multiple times
            gctx.updated_sources().remove(&source.replaced_source_id());
            source.invalidate_cache();
            let mut available = Vec::new();
            for pkg in pkgs {
                if poll_one_package(registry_src, pkg, &mut source)? {
                    available.push(*pkg);
                }
            }

            // As soon as any package is available, break this loop so we can see if another
            // one can be uploaded.
            if !available.is_empty() {
                break available;
            }
        }

        let elapsed = now.elapsed();
        if timeout < elapsed {
            // TODO: update the warning to allow multiple packages
            gctx.shell().warn(format!(
                "timed out waiting for {short_pkg_descriptions} to be available in {source_description}",
            ))?;
            gctx.shell().note(
                "the registry may have a backlog that is delaying making the \
                crate available. The crate should be available soon.",
            )?;
            break Vec::new();
        }

        progress.tick_now(elapsed.as_secs() as usize, max, "")?;
        std::thread::sleep(sleep_time);
    };
    if !available.is_empty() {
        let short_pkg_description = available
            .iter()
            .map(|pkg| format!("{} v{}", pkg.name(), pkg.version()))
            .sorted()
            .join(", ");
        gctx.shell().status(
            "Published",
            format!("{short_pkg_description} at {source_description}"),
        )?;
    }

    Ok(available)
}

fn poll_one_package(
    registry_src: SourceId,
    pkg_id: &PackageId,
    source: &mut dyn Source,
) -> CargoResult<bool> {
    let version_req = format!("={}", pkg_id.version());
    let query = Dependency::parse(pkg_id.name(), Some(&version_req), registry_src)?;
    let summaries = loop {
        // Exact to avoid returning all for path/git
        match source.query_vec(&query, QueryKind::Exact) {
            std::task::Poll::Ready(res) => {
                break res?;
            }
            std::task::Poll::Pending => source.block_until_ready()?,
        }
    };
    Ok(!summaries.is_empty())
}

fn verify_dependencies(
    pkg: &Package,
    registry: &Registry,
    registry_src: SourceId,
) -> CargoResult<()> {
    for dep in pkg.dependencies().iter() {
        if check_dep_has_version(dep, true)? {
            continue;
        }
        // TomlManifest::prepare_for_publish will rewrite the dependency
        // to be just the `version` field.
        if dep.source_id() != registry_src {
            if !dep.source_id().is_registry() {
                // Consider making SourceId::kind a public type that we can
                // exhaustively match on. Using match can help ensure that
                // every kind is properly handled.
                panic!("unexpected source kind for dependency {:?}", dep);
            }
            // Block requests to send to crates.io with alt-registry deps.
            // This extra hostname check is mostly to assist with testing,
            // but also prevents someone using `--index` to specify
            // something that points to crates.io.
            if registry_src.is_crates_io() || registry.host_is_crates_io() {
                bail!("crates cannot be published to crates.io with dependencies sourced from other\n\
                       registries. `{}` needs to be published to crates.io before publishing this crate.\n\
                       (crate `{}` is pulled from {})",
                      dep.package_name(),
                      dep.package_name(),
                      dep.source_id());
            }
        }
    }
    Ok(())
}

pub(crate) fn prepare_transmit(
    gctx: &GlobalContext,
    ws: &Workspace<'_>,
    local_pkg: &Package,
    registry_id: SourceId,
) -> CargoResult<NewCrate> {
    let included = None; // don't filter build-targets
    let publish_pkg = prepare_for_publish(local_pkg, ws, included)?;

    let deps = publish_pkg
        .dependencies()
        .iter()
        .map(|dep| {
            // If the dependency is from a different registry, then include the
            // registry in the dependency.
            let dep_registry_id = match dep.registry_id() {
                Some(id) => id,
                None => SourceId::crates_io(gctx)?,
            };
            // In the index and Web API, None means "from the same registry"
            // whereas in Cargo.toml, it means "from crates.io".
            let dep_registry = if dep_registry_id != registry_id {
                Some(dep_registry_id.url().to_string())
            } else {
                None
            };

            Ok(NewCrateDependency {
                optional: dep.is_optional(),
                default_features: dep.uses_default_features(),
                name: dep.package_name().to_string(),
                features: dep.features().iter().map(|s| s.to_string()).collect(),
                version_req: dep.version_req().to_string(),
                target: dep.platform().map(|s| s.to_string()),
                kind: match dep.kind() {
                    DepKind::Normal => "normal",
                    DepKind::Build => "build",
                    DepKind::Development => "dev",
                }
                .to_string(),
                registry: dep_registry,
                explicit_name_in_toml: dep.explicit_name_in_toml().map(|s| s.to_string()),
                artifact: dep.artifact().map(|artifact| {
                    artifact
                        .kinds()
                        .iter()
                        .map(|x| x.as_str().into_owned())
                        .collect()
                }),
                bindep_target: dep.artifact().and_then(|artifact| {
                    artifact.target().map(|target| target.as_str().to_owned())
                }),
                lib: dep.artifact().map_or(false, |artifact| artifact.is_lib()),
            })
        })
        .collect::<CargoResult<Vec<NewCrateDependency>>>()?;
    let manifest = publish_pkg.manifest();
    let ManifestMetadata {
        ref authors,
        ref description,
        ref homepage,
        ref documentation,
        ref keywords,
        ref readme,
        ref repository,
        ref license,
        ref license_file,
        ref categories,
        ref badges,
        ref links,
        ref rust_version,
    } = *manifest.metadata();
    let rust_version = rust_version.as_ref().map(ToString::to_string);
    let readme_content = local_pkg
        .manifest()
        .metadata()
        .readme
        .as_ref()
        .map(|readme| {
            paths::read(&local_pkg.root().join(readme)).with_context(|| {
                format!("failed to read `readme` file for package `{}`", local_pkg)
            })
        })
        .transpose()?;
    if let Some(ref file) = local_pkg.manifest().metadata().license_file {
        if !local_pkg.root().join(file).exists() {
            bail!("the license file `{}` does not exist", file)
        }
    }

    let string_features = match manifest.normalized_toml().features() {
        Some(features) => features
            .iter()
            .map(|(feat, values)| {
                (
                    feat.to_string(),
                    values.iter().map(|fv| fv.to_string()).collect(),
                )
            })
            .collect::<BTreeMap<String, Vec<String>>>(),
        None => BTreeMap::new(),
    };

    Ok(NewCrate {
        name: publish_pkg.name().to_string(),
        vers: publish_pkg.version().to_string(),
        deps,
        features: string_features,
        authors: authors.clone(),
        description: description.clone(),
        homepage: homepage.clone(),
        documentation: documentation.clone(),
        keywords: keywords.clone(),
        categories: categories.clone(),
        readme: readme_content,
        readme_file: readme.clone(),
        repository: repository.clone(),
        license: license.clone(),
        license_file: license_file.clone(),
        badges: badges.clone(),
        links: links.clone(),
        rust_version,
    })
}

fn transmit(
    gctx: &GlobalContext,
    ws: &Workspace<'_>,
    pkg: &Package,
    tarball: &File,
    registry: &mut Registry,
    registry_id: SourceId,
    dry_run: bool,
) -> CargoResult<()> {
    let new_crate = prepare_transmit(gctx, ws, pkg, registry_id)?;

    // Do not upload if performing a dry run
    if dry_run {
        gctx.shell().warn("aborting upload due to dry run")?;
        return Ok(());
    }

    let warnings = registry
        .publish(&new_crate, tarball)
        .with_context(|| format!("failed to publish to registry at {}", registry.host()))?;

    if !warnings.invalid_categories.is_empty() {
        let msg = format!(
            "the following are not valid category slugs and were \
             ignored: {}. Please see https://crates.io/category_slugs \
             for the list of all category slugs. \
             ",
            warnings.invalid_categories.join(", ")
        );
        gctx.shell().warn(&msg)?;
    }

    if !warnings.invalid_badges.is_empty() {
        let msg = format!(
            "the following are not valid badges and were ignored: {}. \
             Either the badge type specified is unknown or a required \
             attribute is missing. Please see \
             https://doc.rust-lang.org/cargo/reference/manifest.html#package-metadata \
             for valid badge types and their required attributes.",
            warnings.invalid_badges.join(", ")
        );
        gctx.shell().warn(&msg)?;
    }

    if !warnings.other.is_empty() {
        for msg in warnings.other {
            gctx.shell().warn(&msg)?;
        }
    }

    Ok(())
}

// State for tracking dependencies during upload.
struct PublishOrder {
    // The reverse-dependency graph. It has an edge p -> q if package q depends on package p.
    rev_graph: Graph<PackageId, ()>,
    // The weight of a package is the number of unpublished dependencies it has.
    weights: HashMap<PackageId, usize>,
}

impl PublishOrder {
    // Given a package dependency graph, creates a `PublishOrder` for tracking state and also
    // a list of packages that have no dependencies (i.e. can be published immediately).
    fn new<T: Clone + Default>(graph: &Graph<PackageId, T>) -> (Self, Vec<PackageId>) {
        let rev_graph = graph.reversed();

        let weights: HashMap<_, _> = rev_graph
            .iter()
            .map(|id| (*id, graph.edges(id).count()))
            .collect();
        let ready = weights
            .iter()
            .filter_map(|(id, weight)| (*weight == 0).then_some(*id))
            .collect();
        (Self { rev_graph, weights }, ready)
    }

    // Marks a list of packages as having been published, and returns a list of packages
    // that are newly ready for publishing.
    fn mark_published(&mut self, published: Vec<PackageId>) -> Vec<PackageId> {
        let mut ret = Vec::new();
        for id in published {
            for (dependent_id, _) in self.rev_graph.edges(&id) {
                if let Some(weight) = self.weights.get_mut(dependent_id) {
                    *weight = weight.saturating_sub(1);
                    if *weight == 0 {
                        ret.push(*dependent_id);
                    }
                }
            }
        }
        ret
    }
}

// Format a collection of packages as a list, like "foo v0.1.0, bar v0.2.0, and baz v0.3.0".
// The final separator (i.e. "and" in the previous example) can be chosen.
fn package_list(pkgs: impl IntoIterator<Item = PackageId>, final_sep: &str) -> String {
    let mut names: Vec<_> = pkgs
        .into_iter()
        .map(|pkg| format!("`{} v{}`", pkg.name(), pkg.version()))
        .collect();
    names.sort();

    match &names[..] {
        [] => String::new(),
        [a] => a.clone(),
        [a, b] => format!("{a} {final_sep} {b}"),
        [names @ .., last] => {
            format!("{}, {final_sep} {last}", names.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        core::{PackageId, SourceId},
        sources::CRATES_IO_INDEX,
        util::{Graph, IntoUrl},
    };

    use super::PublishOrder;

    fn pkg_id(name: &str) -> PackageId {
        let loc = CRATES_IO_INDEX.into_url().unwrap();
        PackageId::try_new(name, "1.0.0", SourceId::for_registry(&loc).unwrap()).unwrap()
    }

    #[test]
    fn parallel_schedule() {
        let mut graph: Graph<PackageId, ()> = Graph::new();
        let a = pkg_id("a");
        let b = pkg_id("b");
        let c = pkg_id("c");
        let d = pkg_id("d");
        let e = pkg_id("e");

        graph.add(a);
        graph.add(b);
        graph.add(c);
        graph.add(d);
        graph.add(e);
        graph.link(a, c);
        graph.link(b, c);
        graph.link(c, d);
        graph.link(c, e);

        let (mut order, mut ready) = PublishOrder::new(&graph);
        ready.sort();
        assert_eq!(ready, vec![d, e]);

        let ready = order.mark_published(vec![d]);
        assert!(ready.is_empty());

        let ready = order.mark_published(vec![e]);
        assert_eq!(ready, vec![c]);

        let mut ready = order.mark_published(vec![c]);
        ready.sort();
        assert_eq!(ready, vec![a, b]);

        let ready = order.mark_published(vec![a, b]);
        assert!(ready.is_empty());
    }
}
