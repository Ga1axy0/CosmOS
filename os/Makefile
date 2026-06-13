# Building
ARCH ?= riscv64
ARCH_REQUEST := $(strip $(or $(target),$(ARCH),riscv64))
ARCH_NORMALIZED := $(shell printf '%s' "$(ARCH_REQUEST)" | tr '[:upper:]' '[:lower:]')

ifneq ($(filter $(ARCH_NORMALIZED),riscv64 rv64 rv),)
	ARCH := riscv64
else ifneq ($(filter $(ARCH_NORMALIZED),loongarch64 la64 la),)
	ARCH := loongarch64
else
$(error Unsupported target '$(ARCH_REQUEST)'. Use riscv64/rv64/rv or loongarch64/la64/la)
endif

ifeq ($(ARCH),loongarch64)
	TARGET := loongarch64-unknown-none
	QEMU ?= qemu-system-loongarch64
	OBJDUMP := rust-objdump --arch-name=loongarch64
	OBJCOPY := rust-objcopy --binary-architecture=loongarch64
	LOONGARCH_BIOS ?= /usr/local/share/qemu/edk2-loongarch64-code.fd
	LOONGARCH_BOOT_MODE ?= direct
	LOONGARCH_BOOTLOADER := ../bootloader/loongarch64-direct/target/loongarch64-unknown-none/release/loongarch64-direct-boot
	BASE_IMG ?= ../sdcard-la.img
	QEMU_MEMORY ?= 4G
	GDB ?= loongarch64-unknown-elf-gdb
else
	TARGET := riscv64gc-unknown-none-elf
	QEMU ?= qemu-system-riscv64
	OBJDUMP := rust-objdump --arch-name=riscv64
	OBJCOPY := rust-objcopy --binary-architecture=riscv64
	BASE_IMG ?= ../sdcard-rv.img
	QEMU_MEMORY ?= 2G
	GDB ?= riscv64-unknown-elf-gdb
endif
MODE ?= release
KERNEL_ELF = target/$(TARGET)/$(MODE)/os
KERNEL_BIN = $(KERNEL_ELF).bin
DISASM_TMP = target/$(TARGET)/$(MODE)/asm
USER_MODE := release
FS_IMG ?= ../user/target/$(TARGET)/$(USER_MODE)/fs.img
# easyfs or fat32 or ext4
MAIN_FS := ext4
MEM ?= 1G
SMP ?= 1
APPS := ../user/src/bin/*
OFFLINE :=

# BOARD
BOARD := qemu
SBI ?= rustsbi
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

QEMU_PRIMARY_BLK_ARGS = -drive file=$(FS_IMG),if=none,format=raw,id=x0 -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0
QEMU_EXTRA_ARGS ?=

# Building mode argument
MODE_ARG = $(if $(filter release,$(MODE)),--release)

# KERNEL ENTRY
KERNEL_ENTRY_PA := $(if $(filter $(ARCH),loongarch64),0x90000000,0x80200000)
DEBUG_MEMORY ?= 512M
NET_PORT_FORWARD ?= user,id=net0,hostfwd=udp::5555-:5555,hostfwd=udp::5564-:5564,hostfwd=tcp::7777-:7777,hostfwd=tcp::7778-:7778,hostfwd=udp::7778-:7778
QEMU_EXTRA_DEVICES = \
	# -object filter-dump,id=ndump,netdev=net0,file=qemu-net.pcap \
	# -drive file=../second-fat32.img,if=none,format=raw,id=x1 \
	# -device virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1

ifeq ($(ARCH),loongarch64)
QEMU_COMMON_ARGS = -m $(QEMU_MEMORY) -machine virt -cpu la464 -smp $(SMP) -nographic
QEMU_NET_ARGS = \
	-netdev $(NET_PORT_FORWARD) \
	-device virtio-net-pci,netdev=net0,id=net0
QEMU_BLK_ARGS = \
	-drive file=$(FS_IMG),if=none,format=raw,id=x0 \
	-device virtio-blk-pci,drive=x0,id=x0
ifeq ($(LOONGARCH_BOOT_MODE),direct)
QEMU_BOOT_ARGS = -kernel $(LOONGARCH_BOOTLOADER) -device loader,file=$(KERNEL_ELF),addr=$(KERNEL_ENTRY_PA)
else
QEMU_BOOT_ARGS = -bios $(LOONGARCH_BIOS) -device loader,file=$(KERNEL_ELF),addr=$(KERNEL_ENTRY_PA)
endif
GDB_ARCH_EX =
else
QEMU_COMMON_ARGS = -m $(QEMU_MEMORY) -machine virt -smp $(SMP) -nographic
QEMU_NET_ARGS = \
	-netdev $(NET_PORT_FORWARD) \
	-device virtio-net-device,netdev=net0,id=net0
QEMU_BLK_ARGS = \
	-drive file=$(FS_IMG),if=none,format=raw,id=x0 \
	-device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0
QEMU_BOOT_ARGS = -bios $(BOOTLOADER) -device loader,file=$(KERNEL_BIN),addr=$(KERNEL_ENTRY_PA)
GDB_ARCH_EX = -ex 'set arch riscv:rv64'
endif

# Disassembly
DISASM ?= -x

build: env kernel fs-img
	@$(OBJCOPY) $(KERNEL_ELF) --strip-all -O binary $(KERNEL_BIN)

ifeq ($(ARCH),loongarch64)
bootloader:
	@cd ../bootloader/loongarch64-direct && cargo build --release
else
bootloader:
	@true
endif

env:
ifeq ($(OFFLINE),)
	(rustup target list | grep "$(TARGET) (installed)") || rustup target add $(TARGET)
	cargo install cargo-binutils
	rustup component add rust-src
	rustup component add llvm-tools-preview
endif

fs-img:
	@if [ -f "$(FS_IMG)" ]; then \
		echo "Using existing image: $(FS_IMG) (skip user rebuild/repack)."; \
	else \
		echo "Image not found: $(FS_IMG). Building user apps first."; \
		make -C ../user clean; \
		make -C ../user build; \
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
			cd ../fs-fuse && cargo run --release -- -s ../user/target/$(TARGET)/$(USER_MODE)/ -t $(FS_IMG) -f $(MAIN_FS) --ext4-base $(BASE_IMG); \
		else \
			rm -f $(FS_IMG); \
			echo "Packing fresh $(MAIN_FS) image from user apps."; \
			cd ../fs-fuse && cargo run --release -- -s ../user/target/$(TARGET)/$(USER_MODE)/ -t $(FS_IMG) -f $(MAIN_FS); \
		fi; \
	fi

kernel:
	@echo Platform: $(BOARD)
ifeq ($(QEMU7_CFG),--cfg qemu7)
	@LOG=$(LOG) cargo rustc $(MODE_ARG) --target $(TARGET) --no-default-features --features $(MAIN_FS) -- --cfg qemu7
else
	@LOG=$(LOG) cargo build $(MODE_ARG) --target $(TARGET) --no-default-features --features $(MAIN_FS)
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

run: build bootloader run-inner

run-trace: build bootloader run-inner-trace

fast-run: kernel
	@$(OBJCOPY) $(KERNEL_ELF) --strip-all -O binary $(KERNEL_BIN)
	@$(MAKE) run-inner

run-inner:
	@$(QEMU) \
		$(QEMU_COMMON_ARGS) \
		$(QEMU_BOOT_ARGS) \
		$(QEMU_NET_ARGS) \
		$(QEMU_BLK_ARGS)
#	Uncomment above lines to add a second virtio-blk device (for testing mount/umount) 

run-inner-trace:
	@$(QEMU) \
		$(QEMU_COMMON_ARGS) \
		$(QEMU_BOOT_ARGS) \
		$(QEMU_NET_ARGS) \
		$(QEMU_BLK_ARGS) \
		-d int,in_asm \
		-D qemu.log

debug: MODE=debug
debug: build bootloader
	@tmux new-session -d \
		"$(QEMU) -m $(DEBUG_MEMORY) $(filter-out -m $(QEMU_MEMORY),$(QEMU_COMMON_ARGS)) $(QEMU_BOOT_ARGS) $(QEMU_NET_ARGS) $(QEMU_BLK_ARGS) -s -S" && \
		tmux split-window -h "$(GDB) -ex 'file $(KERNEL_ELF)' $(GDB_ARCH_EX) -ex 'target remote localhost:1234'" && \
		tmux -2 attach-session -d


gdbserver: MODE=debug
gdbserver: build bootloader
	@$(QEMU) -m $(DEBUG_MEMORY) $(filter-out -m $(QEMU_MEMORY),$(QEMU_COMMON_ARGS)) $(QEMU_BOOT_ARGS) $(QEMU_NET_ARGS) $(QEMU_BLK_ARGS) \
		-s -S

gdbclient: MODE=debug
gdbclient:
	@$(GDB) -ex 'file $(KERNEL_ELF)' $(GDB_ARCH_EX) -ex 'target remote localhost:1234'

.PHONY: build env kernel clean disasm disasm-vim run run-trace fast-run run-inner fs-img debug gdbserver gdbclient
