//! `boxlite reserve-release` — emergency: free the structural reserve so
//! the operator can run `rm` / `gc` on a host that's hit ENOSPC.
//!
//! The reserve is a preallocated file under `$BOXLITE_HOME/.reserve`
//! that's billed against the host filesystem (see
//! `boxlite::util::reserve`). Releasing it is a metadata-only `unlink`,
//! which works even when the filesystem is at zero free space — exactly
//! the case where this command is useful. The reserve is automatically
//! recreated the next time a runtime constructs (e.g. on the next
//! `boxlite serve` start, `boxlite run`, etc.).
//!
//! Typical recovery flow:
//!
//! ```text
//! $ boxlite run alpine ...
//! Error: No space left on device
//!
//! $ boxlite reserve-release
//! Released 64.0 MiB; run `boxlite gc` / `boxlite rm -f <box>` to reclaim more.
//!
//! $ boxlite gc          # now has 64 MiB of headroom to work in
//! Reclaimed 1.2 GiB ...
//! ```

use clap::Args;

use crate::cli::GlobalFlags;

#[derive(Args, Debug)]
pub struct ReserveReleaseArgs;

pub async fn execute(_args: ReserveReleaseArgs, global: &GlobalFlags) -> anyhow::Result<()> {
    // Resolve the home dir directly — we *cannot* go through
    // `create_runtime()` here, because constructing a runtime calls
    // `ensure_reserve()` which would immediately put the reserve back
    // before we even get a chance to release it. The whole point of
    // this command is to operate on the on-disk file without taking the
    // runtime lock.
    let options = global.resolve_runtime_options()?;
    let released = boxlite::util::release_reserve(&options.home_dir)?;

    if released.bytes == 0 {
        println!(
            "Reserve already released (or never created at {}).",
            options.home_dir.display()
        );
    } else {
        println!(
            "Released {} from {}; run `boxlite gc` / `boxlite rm -f <box>` \
             to reclaim more. The reserve will be recreated automatically \
             on the next runtime start.",
            human_bytes(released.bytes),
            options.home_dir.join(".reserve").display()
        );
    }
    Ok(())
}

fn human_bytes(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else {
        format!("{:.1} MiB", b / MIB)
    }
}
