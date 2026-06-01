use anyhow::Result;
use clap::Args;

use crate::cli::GlobalFlags;

#[derive(Args, Debug)]
pub struct PullArgs {
    /// Image to pull
    pub image: String,

    /// Quiet mode - only show digest
    #[arg(short, long)]
    pub quiet: bool,
}

pub async fn execute(args: PullArgs, global: &GlobalFlags) -> Result<()> {
    let runtime = global.create_runtime()?;

    if runtime.is_rest() {
        // REST path (POL-32). The server pulls and caches the image into
        // *its* layer store; we only get metadata back. `layer_count` and
        // local `config_digest` aren't part of the wire `ImageInfo`, so
        // we render the smaller-but-honest "Pulled: <ref>, ID: <id>" pair
        // — the same id the server returns from `GET /images`.
        let info = runtime.pull_image_remote(&args.image).await?;
        if args.quiet {
            println!("{}", info.id);
        } else {
            println!("Pulled: {}", info.reference);
            println!("ID: {}", info.id);
        }
        return Ok(());
    }

    let images = runtime.images()?;
    let image = images.pull(&args.image).await?;
    if args.quiet {
        println!("{}", image.config_digest());
    } else {
        println!("Pulled: {}", image.reference());
        println!("Digest: {}", image.config_digest());
        println!("Layers: {}", image.layer_count());
    }

    Ok(())
}
