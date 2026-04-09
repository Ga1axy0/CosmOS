# Building
TARGET := riscv64gc-unknown-none-elf
MODE := release
KERNEL_ELF := target/$(TARGET)/$(MODE)/os
KERNEL_BIN := $(KERNEL_ELF).bin
DISASM_TMP := target/$(TARGET)/$(MODE)/asm
FS_IMG := ../user/target/$(TARGET)/$(MODE)/fs.img
# easyfs or fat32 or ext4
MAIN_FS := ext4
LOG ?= INFO
SMP ?= 1
# Optional prebuilt ext4 image path. When set and valid, fs-fuse clones this
# image and injects user apps into it.
BASE_IMG ?= ../sdcard-rv.img
APPS := ../user/src/bin/*
OFFLINE :=

# BOARD
BOARD := qemu
SBI ?= rustsbi

# QEMU
QEMU ?= qemu-system-riscv64
QEMU_VERSION := $(shell $(QEMU) --version 2>/dev/null | head -n1 | sed -E 's/.*version ([0-9.]+).*/\1/')
QEMU_MAJOR := $(shell echo "$(QEMU_VERSION)" | cut -d. -f1)

# Use the newer RustSBI binary only when host QEMU major version is 10.
BOOTLOADER_TAG := 7-0-0
ifeq ($(QEMU_MAJOR),10)
	BOOTLOADER_TAG := 10-1-3
endif
BOOTLOADER := ../bootloader/$(SBI)-$(BOARD)-$(BOOTLOADER_TAG).bin

# Enable cfg(qemu7) only for QEMU 7 or older.
QEMU7_CFG := $(shell echo "$(QEMU_MAJOR)" | awk '/^[0-9]+$$/ { if ($$1 <= 7) print "--cfg qemu7" }')

# Building mode argument
ifeq ($(MODE), release)
	MODE_ARG := --release
endif

# KERNEL ENTRY
KERNEL_ENTRY_PA := 0x80200000

# Binutils
OBJDUMP := rust-objdump --arch-name=riscv64
OBJCOPY := rust-objcopy --binary-architecture=riscv64

# CHAPTER ?= $(shell git rev-parse --abbrev-ref HEAD | sed -E 's/ch([0-9])/\1/')
CHAPTER = 8
TEST ?= $(CHAPTER)
BASE ?= 1

# Disassembly
DISASM ?= -x

# Run usertests or usershell
TEST ?=

build: env $(KERNEL_BIN) fs-img 

env:
ifeq ($(OFFLINE),)
	(rustup target list | grep "riscv64gc-unknown-none-elf (installed)") || rustup target add $(TARGET)
	cargo install cargo-binutils
	rustup component add rust-src
	rustup component add llvm-tools-preview
endif

$(KERNEL_BIN): kernel
	@$(OBJCOPY) $(KERNEL_ELF) --strip-all -O binary $@

fs-img:
	@if [ -f "$(FS_IMG)" ]; then \
		echo "Using existing image: $(FS_IMG) (skip user rebuild/repack)."; \
	else \
		echo "Image not found: $(FS_IMG). Building user apps first."; \
		make -C ../user clean; \
		make -C ../user build TEST=$(TEST) CHAPTER=$(CHAPTER) BASE=$(BASE); \
		if [ -n "$(strip $(BASE_IMG))" ]; then \
			if [ ! -f "$(BASE_IMG)" ]; then \
				echo "Base image not found: $(BASE_IMG)"; \
				exit 2; \
			fi; \
			if [ "$(MAIN_FS)" != "ext4" ]; then \
				echo "BASE_IMG is only supported when MAIN_FS=ext4."; \
				exit 2; \
			fi; \
			echo "Injecting user apps into base ext4 image: $(BASE_IMG)"; \
			cd ../fs-fuse && cargo run --release -- -s ../user/target/riscv64gc-unknown-none-elf/release/ -t $(FS_IMG) -f $(MAIN_FS) --ext4-base $(BASE_IMG); \
		else \
			rm -f $(FS_IMG); \
			echo "Packing fresh $(MAIN_FS) image from user apps."; \
			cd ../fs-fuse && cargo run --release -- -s ../user/target/riscv64gc-unknown-none-elf/release/ -t $(FS_IMG) -f $(MAIN_FS); \
		fi; \
	fi

kernel:
	@echo Platform: $(BOARD)
ifeq ($(QEMU7_CFG),--cfg qemu7)
	@LOG=$(LOG) cargo rustc $(MODE_ARG) --no-default-features --features $(MAIN_FS) -- --cfg qemu7
else
	@LOG=$(LOG) cargo build $(MODE_ARG) --no-default-features --features $(MAIN_FS)
endif

clean:
	@cargo clean
	@rm -f $(FS_IMG)

disasm: kernel
	@$(OBJDUMP) $(DISASM) $(KERNEL_ELF) | less

disasm-vim: kernel
	@$(OBJDUMP) $(DISASM) $(KERNEL_ELF) > $(DISASM_TMP)
	@vim $(DISASM_TMP)
	@rm $(DISASM_TMP)

run: build run-inner

run-trace: build run-inner-trace

fast-run: kernel $(KERNEL_BIN) run-inner

run-inner:
	@$(QEMU) \
		-M 128m \
		-machine virt \
		-smp $(SMP) \
		-nographic \
		-bios $(BOOTLOADER) \
		-device loader,file=$(KERNEL_BIN),addr=$(KERNEL_ENTRY_PA) \
		-netdev user,id=net0,hostfwd=udp::5555-:5555,hostfwd=udp::5564-:5564,hostfwd=tcp::7777-:7777 \
        -object filter-dump,id=ndump,netdev=net0,file=qemu-net.pcap \
        -device virtio-net-device,netdev=net0,id=net0 \
		-drive file=$(FS_IMG),if=none,format=raw,id=x0 \
        -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
		# -drive file=../second-fat32.img,if=none,format=raw,id=x1 \
		# -device virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1
#	Uncomment above lines to add a second virtio-blk device (for testing mount/umount) 

run-inner-trace:
	@$(QEMU) \
		-M 128m \
		-machine virt \
		-smp $(SMP) \
		-nographic \
		-bios $(BOOTLOADER) \
		-device loader,file=$(KERNEL_BIN),addr=$(KERNEL_ENTRY_PA) \
		-netdev user,id=net0,hostfwd=udp::5555-:5555,hostfwd=udp::5564-:5564,hostfwd=tcp::7777-:7777 \
        -object filter-dump,id=ndump,netdev=net0,file=qemu-net.pcap \
        -device virtio-net-device,netdev=net0,id=net0 \
		-drive file=$(FS_IMG),if=none,format=raw,id=x0 \
        -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
		-d int,in_asm \
		-D qemu.log \
		# -drive file=../second-fat32.img,if=none,format=raw,id=x1 \
		# -device virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1
#	Uncomment above lines to add a second virtio-blk device (for testing mount/umount) 

debug: build
	@tmux new-session -d \
		"$(QEMU) -machine virt -smp $(SMP) -nographic -bios $(BOOTLOADER) -device loader,file=$(KERNEL_BIN),addr=$(KERNEL_ENTRY_PA) -s -S" && \
		tmux split-window -h "riscv64-unknown-elf-gdb -ex 'file $(KERNEL_ELF)' -ex 'set arch riscv:rv64' -ex 'target remote localhost:1234'" && \
		tmux -2 attach-session -d


gdbserver: build
	@$(QEMU) -M 128m -machine virt -smp $(SMP) -nographic -bios $(BOOTLOADER) -device loader,file=$(KERNEL_BIN),addr=$(KERNEL_ENTRY_PA) \
		-drive file=$(FS_IMG),if=none,format=raw,id=x0 \
	        -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
		-s -S

gdbclient:
	@riscv64-unknown-elf-gdb -ex 'file $(KERNEL_ELF)' -ex 'set arch riscv:rv64' -ex 'target remote localhost:1234'

.PHONY: build env kernel clean disasm disasm-vim run-inner fs-img gdbserver gdbclient
