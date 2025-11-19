//! Basic backup example script
//!
//! This example demonstrates how to use zesty-backup via command-line.
//! Since zesty-backup is a binary crate, this shows the equivalent commands.
//!
//! For programmatic usage, you would need to use zesty-backup as a library
//! or call it as a subprocess.

fn main() {
    println!("zesty-backup Usage Examples");
    println!("==========================\n");

    println!("1. Create a backup:");
    println!("   zesty-backup backup\n");

    println!("2. Create a full backup:");
    println!("   zesty-backup backup --full\n");

    println!("3. Upload backups to cloud:");
    println!("   zesty-backup upload\n");

    println!("4. Upload a specific backup:");
    println!("   zesty-backup upload --file backups/backup-20240101-120000.tar.zst\n");

    println!("5. List local backups:");
    println!("   zesty-backup list\n");

    println!("6. List remote backups:");
    println!("   zesty-backup list --remote\n");

    println!("7. Download a backup:");
    println!("   zesty-backup download --key backup-20240101-120000.tar.zst --output ./restored.tar.zst\n");

    println!("8. Run as daemon:");
    println!("   zesty-backup daemon --backup-interval 6 --upload-interval 24 --pid-file /var/run/zesty-backup.pid\n");

    println!("9. Client mode (download from remote):");
    println!("   zesty-backup client --provider s3 --bucket my-bucket --access-key KEY --secret-key SECRET download --key backup.tar.zst --output ./backup.tar.zst\n");

    println!("10. Generate example config:");
    println!("    zesty-backup generate-config --output config.example.toml\n");

    println!("For more information, see README.md and config.example.toml");
}
