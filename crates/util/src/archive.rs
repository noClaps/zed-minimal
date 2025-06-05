use std::path::Path;

use anyhow::{Context as _, Result};
use async_zip::base::read;
use futures::{AsyncRead, io::BufReader};

pub async fn extract_zip<R: AsyncRead + Unpin>(destination: &Path, reader: R) -> Result<()> {
    // Unix needs file permissions copied when extracting.
    // This is only possible to do when a reader impls `AsyncSeek` and `seek::ZipFileReader` is used.
    // `stream::ZipFileReader` also has the `unix_permissions` method, but it will always return `Some(0)`.
    //
    // A typical `reader` comes from a streaming network response, so cannot be sought right away,
    // and reading the entire archive into the memory seems wasteful.
    //
    // So, save the stream into a temporary file first and then get it read with a seeking reader.
    let mut file = async_fs::File::from(tempfile::tempfile().context("creating a temporary file")?);
    futures::io::copy(&mut BufReader::new(reader), &mut file)
        .await
        .context("saving archive contents into the temporary file")?;
    let mut reader = read::seek::ZipFileReader::new(BufReader::new(file))
        .await
        .context("reading the zip archive")?;
    let destination = &destination
        .canonicalize()
        .unwrap_or_else(|_| destination.to_path_buf());
    for (i, entry) in reader.file().entries().to_vec().into_iter().enumerate() {
        let path = destination.join(
            entry
                .filename()
                .as_str()
                .context("reading zip entry file name")?,
        );

        if entry
            .dir()
            .with_context(|| format!("reading zip entry metadata for path {path:?}"))?
        {
            std::fs::create_dir_all(&path)
                .with_context(|| format!("creating directory {path:?}"))?;
        } else {
            let parent_dir = path
                .parent()
                .with_context(|| format!("no parent directory for {path:?}"))?;
            std::fs::create_dir_all(parent_dir)
                .with_context(|| format!("creating parent directory {parent_dir:?}"))?;
            let mut file = smol::fs::File::create(&path)
                .await
                .with_context(|| format!("creating file {path:?}"))?;
            let mut entry_reader = reader
                .reader_with_entry(i)
                .await
                .with_context(|| format!("reading entry for path {path:?}"))?;
            futures::io::copy(&mut entry_reader, &mut file)
                .await
                .with_context(|| format!("extracting into file {path:?}"))?;

            if let Some(perms) = entry.unix_permissions() {
                use std::os::unix::fs::PermissionsExt;
                let permissions = std::fs::Permissions::from_mode(u32::from(perms));
                file.set_permissions(permissions)
                    .await
                    .with_context(|| format!("setting permissions for file {path:?}"))?;
            }
        }
    }

    Ok(())
}
