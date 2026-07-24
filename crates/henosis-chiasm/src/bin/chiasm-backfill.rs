//! `chiasm-backfill` imports legacy Chiasm data into a tenant-scoped store. It is dry-run by
//! default; pass `--apply` to write. Run it against a copy of a source database and inspect the
//! dry-run report before applying anywhere.
//!
//! Each imported database has its own legacy id space, so every run names its source
//! (e.g. `monolith`, `tenant-1`); idempotency is per source.
//!
//! ```text
//! chiasm-backfill --legacy <kleos.sqlite> --target <chiasm.sqlite> \
//!                 --directory <identity.sqlite> --tenant <uuid|new> \
//!                 --source <label> [--apply]
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use henosis_chiasm::backfill::{backfill_from_kleos, BackfillOptions};
use syntheos_contracts::TenantId;
use syntheos_identity::SqliteDirectory;

/// Parsed command-line arguments.
struct Args {
    /// Path to the legacy Kleos SQLite database (opened read-only).
    legacy: PathBuf,
    /// Path to the Henosis chiasm SQLite database (created/migrated as needed).
    target: PathBuf,
    /// Path to the syntheos-identity SqliteDirectory database.
    directory: PathBuf,
    /// Operator-chosen label of the source database (per-source id space + idempotency).
    source: String,
    /// Tenant every imported task is homed under.
    tenant: TenantId,
    /// Whether the tenant id was freshly minted by `--tenant new` (printed so it is not lost).
    tenant_minted: bool,
    /// False = dry run (default), true = write.
    apply: bool,
}

/// Parse argv; `Err` carries the usage/diagnostic message.
fn parse_args() -> Result<Args, String> {
    const USAGE: &str = "usage: chiasm-backfill --legacy <kleos.sqlite> --target <chiasm.sqlite> \
                         --directory <identity.sqlite> --tenant <uuid|new> \
                         --source <label> [--apply]";
    let mut legacy = None;
    let mut target = None;
    let mut directory = None;
    let mut source = None;
    let mut tenant = None;
    let mut tenant_minted = false;
    let mut apply = false;
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        // Every value-taking flag pulls its value from the next argument.
        let mut value = |flag: &str| {
            argv.next()
                .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))
        };
        match arg.as_str() {
            "--legacy" => legacy = Some(PathBuf::from(value("--legacy")?)),
            "--target" => target = Some(PathBuf::from(value("--target")?)),
            "--directory" => directory = Some(PathBuf::from(value("--directory")?)),
            "--source" => source = Some(value("--source")?),
            "--tenant" => {
                let v = value("--tenant")?;
                tenant = Some(if v == "new" {
                    tenant_minted = true;
                    TenantId::new()
                } else {
                    v.parse::<TenantId>()
                        .map_err(|e| format!("bad --tenant {v:?}: {e}"))?
                });
            }
            "--apply" => apply = true,
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    Ok(Args {
        legacy: legacy.ok_or_else(|| format!("--legacy is required\n{USAGE}"))?,
        target: target.ok_or_else(|| format!("--target is required\n{USAGE}"))?,
        directory: directory.ok_or_else(|| format!("--directory is required\n{USAGE}"))?,
        source: source.ok_or_else(|| {
            format!("--source is required (e.g. 'monolith', 'tenant-1')\n{USAGE}")
        })?,
        tenant: tenant
            .ok_or_else(|| format!("--tenant is required (a UUID, or 'new')\n{USAGE}"))?,
        tenant_minted,
        apply,
    })
}

/// Entry point: parse, run on a small current-thread runtime, print the report.
fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to start runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(async {
        let directory = SqliteDirectory::open(&args.directory)
            .map_err(|e| format!("open directory {:?}: {e}", args.directory))?;
        backfill_from_kleos(
            &args.legacy,
            &args.target,
            &directory,
            args.tenant,
            &args.source,
            BackfillOptions {
                dry_run: !args.apply,
            },
        )
        .await
        .map_err(|e| e.to_string())
    });
    match result {
        Ok(report) => {
            let mode = if report.dry_run { "DRY RUN" } else { "APPLIED" };
            println!("chiasm-backfill {mode}");
            println!("  source: {}", report.source);
            if args.tenant_minted {
                println!("  tenant (newly minted -- record this): {}", args.tenant);
            } else {
                println!("  tenant: {}", args.tenant);
            }
            println!("  principals minted: {}", report.principals_minted);
            for (legacy_key, principal) in &report.principals_by_legacy_user {
                println!("    legacy key {legacy_key} -> {principal}");
            }
            println!("  tasks imported:    {}", report.tasks_imported);
            println!("  tasks skipped:     {}", report.tasks_skipped);
            println!("  history imported:  {}", report.updates_imported);
            println!("  deps imported:     {}", report.dependencies_imported);
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprintln!("backfill failed: {msg}");
            ExitCode::FAILURE
        }
    }
}
