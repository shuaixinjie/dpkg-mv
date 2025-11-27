use ar::{Archive, Builder};
use clap::Parser;
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use std::{
    fs::File,
    io::{self, Read, Write},
    path::PathBuf,
};
use tar::{Archive as TarArchive, Builder as TarBuilder};
use xz2::{read::XzDecoder, write::XzEncoder};

// --- Command line arguments struct ---

#[derive(Parser, Debug)]
#[command(author, version, about = "A tool to replace a file inside a Debian (.deb) package.", long_about = None)]
struct Args {
    /// Path to the new file used for replacement
    #[arg(index = 1)]
    new_file: PathBuf,

    /// Path to the target .deb package
    #[arg(index = 2)]
    deb_path: PathBuf,

    /// Path of the file inside the deb package to replace (e.g., /opt/apps/cc/abc)
    #[arg(index = 3)]
    target_path: String,
}

// --- Compression type enum ---
#[derive(Clone, Copy, Debug)]
enum CompressionType {
    None,
    Gzip,
    Xz,
    // TODO: Add Bzip2, Zstd support here in the future
}

// --- Helper function: Get decoder ---

/// Return the correct decoder and compression type based on filename extension
fn get_decoder<'a>(
    data: &'a [u8],
    filename: &str,
) -> io::Result<(Box<dyn Read + 'a>, CompressionType)> {
    if filename.ends_with(".gz") {
        Ok((Box::new(GzDecoder::new(data)), CompressionType::Gzip))
    } else if filename.ends_with(".xz") {
        Ok((Box::new(XzDecoder::new(data)), CompressionType::Xz))
    } else {
        // Assume uncompressed tar (data.tar)
        Ok((Box::new(io::Cursor::new(data)), CompressionType::None))
    }
}

// --- Helper function: Get encoder ---

/// Recompress data based on compression type
fn get_encoder(data: &[u8], compression_type: CompressionType) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    match compression_type {
        CompressionType::Gzip => {
            let mut encoder = GzEncoder::new(&mut output, Compression::best());
            encoder.write_all(data)?;
            encoder.finish()?;
        }
        CompressionType::Xz => {
            let mut encoder = XzEncoder::new(&mut output, 9); // level 9 (best)
            encoder.write_all(data)?;
            encoder.finish()?;
        }
        CompressionType::None => {
            output.extend_from_slice(data);
        }
    }
    Ok(output)
}

// --- Core logic function: Replace file ---

fn replace_file_in_deb(
    new_file_path: &PathBuf,
    deb_path: &PathBuf,
    target_path_in_deb: &str,
) -> io::Result<()> {
    // 1. Read the new file contents
    let new_file_data = std::fs::read(new_file_path)?;

    // 2. Prepare temp .deb output path
    let temp_deb_path = deb_path.with_extension("new.deb.temp");
    let mut new_deb_builder = Builder::new(File::create(&temp_deb_path)?);

    // 3. Open original deb file and prepare for reading
    let mut original_archive = Archive::new(File::open(deb_path)?);
    let mut data_archive_bytes: Option<Vec<u8>> = None;
    let mut data_filename = String::new();
    let temp_target_path = target_path_in_deb.trim_start_matches("./");
    let normalized_target_path = temp_target_path.trim_start_matches('/');

    // 4. Iterate through original .deb (ar) archive
    println!("🔍 Analyzing {}...", deb_path.display());
    while let Some(entry) = original_archive.next_entry() {
        let mut entry = entry?;

        // Convert filename bytes to string
        let filename_bytes = entry.header().identifier();
        let filename = std::str::from_utf8(filename_bytes)
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Invalid ar filename encoding: {}", e),
                )
            })?
            .trim_end()
            .to_string();

        if filename.starts_with("data.tar") {
            // Found data.tar.*, save its contents
            data_filename = filename;
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            data_archive_bytes = Some(buf);
        } else {
            // Copy debian-binary and control.tar.* to new deb
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            new_deb_builder.append(entry.header(), buf.as_slice())?;
            println!("   -> Copied: {}", filename);
        }
    }

    let data_archive_bytes = data_archive_bytes.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "data.tar archive not found in deb package",
        )
    })?;

    // 5. Decompress data.tar archive
    let (data_reader, compression_type) = get_decoder(&data_archive_bytes, &data_filename)?;

    // 6. Prepare to build new data.tar archive
    let mut new_data_tar_buf = Vec::new();
    let mut found_and_replaced = false;

    // Use a separate scope to avoid mutable borrow issues
    {
        let mut new_data_tar_builder = TarBuilder::new(&mut new_data_tar_buf);
        let mut tar_archive = TarArchive::new(data_reader);

        // 7. Traverse original data.tar and perform replacement
        for entry_result in tar_archive.entries()? {
            let mut entry = entry_result?;

            let entry_path = entry.path()?.to_string_lossy().into_owned();
            let temp_path = entry_path.trim_start_matches("./");
            let normalized_entry_path = temp_path.trim_start_matches('/');

            if normalized_entry_path == normalized_target_path {
                // Match target file → Replace it
                let mut header = entry.header().clone();
                let original_size = header.size()?;

                header.set_size(new_file_data.len() as u64);

                new_data_tar_builder.append_data(
                    &mut header,
                    &entry_path,
                    new_file_data.as_slice(),
                )?;

                found_and_replaced = true;
                println!(
                    "   -> ⭐ Replaced: {} (Size: {} -> {})",
                    entry_path,
                    original_size,
                    new_file_data.len()
                );
            } else {
                // Non-target file → Copy as-is
                let mut header_clone = entry.header().clone();
                new_data_tar_builder.append_data(&mut header_clone, &entry_path, &mut entry)?;
            }
        }

        new_data_tar_builder.finish()?;
    }

    if !found_and_replaced {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "Target file '{}' not found in data archive.",
                target_path_in_deb
            ),
        ));
    }

    // 8. Recompress new data.tar
    let recompressed_data_bytes = get_encoder(&new_data_tar_buf, compression_type)?;
    println!(
        "   -> Recompressed data archive (Type: {:?})",
        compression_type
    );

    // 9. Write new data.tar.* into deb archive
    let mut new_data_ar_header = ar::Header::new(
        data_filename.clone().into_bytes(),
        recompressed_data_bytes.len() as u64,
    );

    new_data_ar_header.set_mode(0o644);

    new_deb_builder.append(&new_data_ar_header, recompressed_data_bytes.as_slice())?;

    // 10. Replace old deb atomically
    drop(new_deb_builder);

    std::fs::rename(&temp_deb_path, deb_path)?;
    println!(
        "\n✅ Replacement complete. New package saved as {}",
        deb_path.display()
    );

    Ok(())
}

// --- Main function ---

fn main() {
    let args = Args::parse();

    if let Err(e) = replace_file_in_deb(&args.new_file, &args.deb_path, &args.target_path) {
        eprintln!("\n❌ Error processing deb package: {}", e);
        std::process::exit(1);
    }
}
