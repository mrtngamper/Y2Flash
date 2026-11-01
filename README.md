# AI slop ahead

Large parts of this project were build using LLMs. If you are not fond of this kind of work,
you might want to move on now.

# Y2Flash

Flash a MediaTek firmware image (a folder, a `.zip` or a scatter file) to a
device over USB. It is built for the Innioasis Y2 (MT6582). The protocol is ported from [mtkclient](https://github.com/bkerler/mtkclient), which also works fine,
if not better for this exact process. 

## Use

```sh
cargo run --release -- software                     # what can be installed on a Y2 (online catalogue)
cargo run --release -- releases rockbox             # its releases and firmware files
cargo run --release -- download rockbox             # newest release, verified, into ~/.cache/y2flash
cargo run --release -- flash --software rockbox --release v0.2.5-beta --dry-run
cargo run --release -- flash rom_y2.zip --dry-run   # show what would be written
cargo run --release -- flash rom_y2.zip             # then power off the player and plug it in
cargo run --release -- flash rom_y2.zip --only BOOTIMG,RECOVERY
cargo run --release -- flash --software solar --repartition   # other layout, erases internal storage
cargo run --release -- info                         # chip, eMMC sizes and partition table, writes nothing
cargo run --release -- info rom_y2.zip              # same, names partitions, also works in BootROM mode
sudo target/release/y2flash mount 2 /mnt/partition   # mount partition 2 from `info`. Use Ctrl-C to unmount
```

On Linux your user must be in the `dialout` group to open the device.