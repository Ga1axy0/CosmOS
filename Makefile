DOCKER_NAME ?= rcore-docker

ARCH ?= rv
BUILD_ARCH ?= all
TARGET ?= riscv64gc-unknown-none-elf
USER_MODE ?= release
USER_BIN_DIR := user/target/$(TARGET)/$(USER_MODE)
USER_BIN_DIR_RV := user/target/riscv64gc-unknown-none-elf/$(USER_MODE)
USER_BIN_DIR_LA := user/target/loongarch64-unknown-none/$(USER_MODE)
KERNEL_RV_ELF := os/target/$(TARGET)/release/os
KERNEL_LA_ELF := os/target/loongarch64-unknown-none/release/os
QEMU_RV ?= qemu-system-riscv64
QEMU_LA ?= qemu-system-loongarch64
MEM ?= 1G
SMP ?= 1
TEST_FS ?= sdcard-$(ARCH).img
# make run 使用写时复制副本，避免 QEMU 写坏原始测试镜像。
RUN_TEST_FS ?= .make/sdcard-$(ARCH)-run.img
TEST_FS_LA ?= sdcard-la.img
RUN_TEST_FS_LA ?= .make/sdcard-la-run.img
QEMU_NETDEV ?= user,id=net
FAST_RUN_QEMU_NETDEV ?= user,id=net,hostfwd=tcp::7777-:7777
QEMU_TRACE_ARGS ?=
QEMU_COMP_BLK_ARGS = -drive file=$(RUN_TEST_FS),if=none,format=raw,id=x0 -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0
QEMU_COMP_EXTRA_BLK_ARGS = -drive file=$(RUN_DISK_IMG),if=none,format=raw,id=x1 -device virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1

STAMP_DIR := .make
USER_BUILD_STAMP_RV := $(USER_BIN_DIR_RV)/.xxos-build.stamp
USER_BUILD_STAMP_LA := $(USER_BIN_DIR_LA)/.xxos-build.stamp
KERNEL_BUILD_STAMP_RV := $(STAMP_DIR)/kernel-build-rv.stamp
KERNEL_BUILD_STAMP_LA := $(STAMP_DIR)/kernel-build-la.stamp
KERNEL_LOG_KEY := $(if $(strip $(LOG)),$(strip $(LOG)),OFF)
KERNEL_LOG_STAMP_RV := $(STAMP_DIR)/kernel-log-rv-$(KERNEL_LOG_KEY).stamp
KERNEL_LOG_STAMP_LA := $(STAMP_DIR)/kernel-log-la-$(KERNEL_LOG_KEY).stamp
USER_BUILD_DEPS := user/Makefile user/Cargo.toml $(shell find user/src -type f | sort)
KERNEL_BUILD_DEPS := os/Makefile os/Cargo.toml os/build.rs $(shell find os/src fs/src -type f | sort)
LA_BOOTLOADER_DIR ?= bootloader/loongarch64-direct
LA_BOOTLOADER_DEPS := $(LA_BOOTLOADER_DIR)/Cargo.toml $(LA_BOOTLOADER_DIR)/Cargo.lock $(LA_BOOTLOADER_DIR)/build.rs $(LA_BOOTLOADER_DIR)/linker.ld $(shell find $(LA_BOOTLOADER_DIR)/src -type f | sort)
ROOTFS_REPO := CosmOS-rootfs
ROOTFS_BASE_DIR := $(ROOTFS_REPO)/rootfs
ROOTFS_RV_DIR := $(ROOTFS_REPO)/rootfs-rv
ROOTFS_LA_DIR := $(ROOTFS_REPO)/rootfs-la
ROOTFS_RV_BUILD_DIR := $(ROOTFS_REPO)/build/rv
ROOTFS_LA_BUILD_DIR := $(ROOTFS_REPO)/build/la
ROOTFS_RV_STAMP_DIR := $(ROOTFS_REPO)/build/.stamps-rv
ROOTFS_LA_STAMP_DIR := $(ROOTFS_REPO)/build/.stamps-la
ROOTFS_RV_FILES := $(shell if [ -d $(ROOTFS_RV_DIR) ]; then find $(ROOTFS_RV_DIR) -type f | sort; fi)
ROOTFS_LA_FILES := $(shell if [ -d $(ROOTFS_LA_DIR) ]; then find $(ROOTFS_LA_DIR) -type f | sort; fi)
DISK_RV_IMG := disk.img
DISK_LA_IMG := disk-la.img
QEMU_LA_BLK_ARGS = -drive file=$(RUN_TEST_FS_LA),if=none,format=raw,id=x0 -device virtio-blk-pci,drive=x0,id=x0
QEMU_LA_EXTRA_BLK_ARGS = -drive file=$(DISK_LA_IMG),if=none,format=raw,id=x1 -device virtio-blk-pci,drive=x1,id=x1
RV_ROOTFS_TARGET ?= riscv64-linux-musl
RV_TOOLCHAIN_BIN ?= /opt/riscv64-linux-musl-cross/bin
RV_GLIBC_LIB ?= /usr/riscv64-linux-gnu/lib
RV_MUSL_LIB ?= /opt/riscv64-linux-musl-cross/riscv64-linux-musl/lib
RV_MUSL_ARCH ?= riscv64
RV_MUSL_LOADER_ALIASES ?= ld-musl-riscv64.so.1 ld-musl-riscv64-sf.so.1
LA_ROOTFS_TARGET ?= loongarch64-linux-musl
LA_TOOLCHAIN_BIN ?= /opt/loongarch64-linux-musl-cross/bin
LA_GLIBC_TOOLCHAIN ?= /opt/gcc-13.2.0-loongarch64-linux-gnu
LA_MUSL_LIB ?= /opt/loongarch64-linux-musl-cross/loongarch64-linux-musl/lib
LA_MUSL_ARCH ?= loongarch64
LA_MUSL_LOADER_ALIASES ?= ld-musl-loongarch64.so.1
LA_BOOTLOADER_ELF ?= $(LA_BOOTLOADER_DIR)/target/loongarch64-unknown-none/release/loongarch64-direct-boot
LA_KERNEL_ENTRY_PA ?= 0x90000000
MEM_LA ?= 1G
QEMU_LA_NETDEV ?= user,id=net0
OPTIONAL_RUNTIME_FILES := $(wildcard lib/musl/ar lib/glibc/ar)

# make all 默认同时构建两种架构；命令行可用 BUILD_ARCH/ARCH 只选一种。
ifeq ($(origin BUILD_ARCH),command line)
ALL_ARCH_REQUEST := $(BUILD_ARCH)
else ifeq ($(origin ARCH),command line)
ALL_ARCH_REQUEST := $(ARCH)
else
ALL_ARCH_REQUEST := all
endif
ALL_ARCH := $(shell printf '%s' "$(ALL_ARCH_REQUEST)" | tr '[:upper:]' '[:lower:]')

ALL_TARGETS_RV := user-apps kernel-rv $(DISK_RV_IMG)
ALL_TARGETS_LA := user-apps-la kernel-la $(DISK_LA_IMG)

ifneq ($(filter $(ALL_ARCH),all both),)
ALL_BUILD_TARGETS := $(ALL_TARGETS_RV) $(ALL_TARGETS_LA)
else ifneq ($(filter $(ALL_ARCH),rv riscv riscv64 rv64),)
ALL_BUILD_TARGETS := $(ALL_TARGETS_RV)
else ifneq ($(filter $(ALL_ARCH),la loongarch loongarch64 la64),)
ALL_BUILD_TARGETS := $(ALL_TARGETS_LA)
else
$(error unsupported BUILD_ARCH/ARCH=$(ALL_ARCH_REQUEST), expected all, rv or la)
endif

ifeq ($(ARCH),rv)
QEMU ?= $(QEMU_RV)
RUN_KERNEL := kernel-rv
RUN_DISK_IMG := $(DISK_RV_IMG)
else ifeq ($(ARCH),la)
QEMU ?= $(QEMU_LA)
RUN_KERNEL := kernel-la
RUN_DISK_IMG := $(DISK_LA_IMG)
else
$(error unsupported ARCH=$(ARCH), expected rv or la)
endif

.PHONY: all submodules cargo-config docker build_docker fmt user-apps rootfs sync-rootfs-variants rootfs-rv rootfs-la rv la disk-rv disk-la clean run run-trace run-comp-rv run-comp-la fast-run fast-run-la clean-all debug gdbserver gdbclient check-kernel check-user-apps check-rootfs check-rootfs-rv check-rootfs-la check-rootfs-rv-ready check-rootfs-la-ready prepare-run-test-fs prepare-run-test-fs-la force

all:
	$(MAKE) submodules
	$(MAKE) cargo-config
	$(MAKE) $(ALL_BUILD_TARGETS)

# 拉取所有子模块，确保后续构建依赖完整。
submodules:
	@if [ -f .gitmodules ]; then \
		git submodule update --init --recursive; \
	else \
		echo "No .gitmodules found; assuming dependencies are already vendored."; \
	fi

# 评测会过滤隐藏目录，构建前从非隐藏目录恢复 Cargo 配置。
cargo-config:
	@mkdir -p os/.cargo user/.cargo
	@cp os/cargo-config/config.toml os/.cargo/config.toml
	@cp user/cargo-config/config.toml user/.cargo/config.toml

$(STAMP_DIR):
	mkdir -p $@

$(USER_BUILD_STAMP_RV): $(USER_BUILD_DEPS)
	$(MAKE) -C user build ARCH=riscv64
	touch $@

$(USER_BUILD_STAMP_LA): $(USER_BUILD_DEPS)
	$(MAKE) -C user build ARCH=loongarch64
	touch $@

user-apps: $(USER_BUILD_STAMP_RV)
user-apps-la: $(USER_BUILD_STAMP_LA)

$(KERNEL_LOG_STAMP_RV): | $(STAMP_DIR)
	touch $@

$(KERNEL_LOG_STAMP_LA): | $(STAMP_DIR)
	touch $@

$(KERNEL_BUILD_STAMP_RV): $(KERNEL_BUILD_DEPS) $(KERNEL_LOG_STAMP_RV) | $(STAMP_DIR)
	$(MAKE) -C os kernel ARCH=riscv64
	touch $@

$(KERNEL_BUILD_STAMP_LA): $(KERNEL_BUILD_DEPS) $(KERNEL_LOG_STAMP_LA) | $(STAMP_DIR)
	$(MAKE) -C os kernel ARCH=loongarch64
	touch $@

kernel-rv: $(KERNEL_BUILD_STAMP_RV)
	cp $(KERNEL_RV_ELF) $@

kernel-la: $(KERNEL_BUILD_STAMP_LA)
	cp $(KERNEL_LA_ELF) $@

$(ROOTFS_STAMP): $(ROOTFS_SRC_FILES) | $(STAMP_DIR)
	rm -rf $(ROOTFS_DIR)
	cp -a $(ROOTFS_SRC_DIR)/rootfs/. $(ROOTFS_DIR)/
	touch $@

sync-rootfs-variants:
	@test -d "$(ROOTFS_BASE_DIR)" || { \
		echo "missing base rootfs directory $(ROOTFS_BASE_DIR); run 'make rootfs' if you need to build it" >&2; \
		exit 1; \
	}
	@test -d "$(ROOTFS_BASE_DIR)/root" || { \
		echo "base rootfs is incomplete under $(ROOTFS_BASE_DIR)" >&2; \
		exit 1; \
	}
	@for dir in "$(ROOTFS_RV_DIR)" "$(ROOTFS_LA_DIR)"; do \
		echo "[SYNC] $(ROOTFS_BASE_DIR) -> $$dir"; \
		mkdir -p "$$dir"; \
		cp -a "$(ROOTFS_BASE_DIR)"/. "$$dir"/; \
	done

rootfs-rv: sync-rootfs-variants
	$(MAKE) -C $(ROOTFS_REPO) rootfs-init \
		ROOTFS_DIR="$(CURDIR)/$(ROOTFS_RV_DIR)" \
		BUILD_ROOT="$(CURDIR)/$(ROOTFS_RV_BUILD_DIR)" \
		STAMP_DIR="$(CURDIR)/$(ROOTFS_RV_STAMP_DIR)" \
		TARGET=$(RV_ROOTFS_TARGET) \
		TOOLCHAIN_BIN=$(RV_TOOLCHAIN_BIN) \
		BUSYBOX_ARCH=riscv \
		GLIBC_LIB=$(RV_GLIBC_LIB) \
		MUSL_LIB=$(RV_MUSL_LIB) \
		MUSL_ARCH=$(RV_MUSL_ARCH) \
		MUSL_LOADER_ALIASES="$(RV_MUSL_LOADER_ALIASES)"

rootfs-la: sync-rootfs-variants
	$(MAKE) -C $(ROOTFS_REPO) rootfs-init \
		ROOTFS_DIR="$(CURDIR)/$(ROOTFS_LA_DIR)" \
		BUILD_ROOT="$(CURDIR)/$(ROOTFS_LA_BUILD_DIR)" \
		STAMP_DIR="$(CURDIR)/$(ROOTFS_LA_STAMP_DIR)" \
		TARGET=$(LA_ROOTFS_TARGET) \
		TOOLCHAIN_BIN=$(LA_TOOLCHAIN_BIN) \
		BUSYBOX_ARCH=loongarch \
		GLIBC_TOOLCHAIN=$(LA_GLIBC_TOOLCHAIN) \
		MUSL_LIB=$(LA_MUSL_LIB) \
		MUSL_ARCH=$(LA_MUSL_ARCH) \
		MUSL_LOADER_ALIASES="$(LA_MUSL_LOADER_ALIASES)"

rv disk-rv: $(DISK_RV_IMG)

la disk-la: $(DISK_LA_IMG)

check-kernel: $(RUN_KERNEL)
	@test -x "$(RUN_KERNEL)" || { \
		echo "missing $(RUN_KERNEL); run 'make all' first" >&2; \
		exit 1; \
	}

check-kernel-la: kernel-la
	@test -x kernel-la || { \
		echo "missing kernel-la; run 'make all' first" >&2; \
		exit 1; \
	}

check-user-apps-rv: user-apps
	@test -d "$(USER_BIN_DIR_RV)" || { \
		echo "missing user binaries in $(USER_BIN_DIR_RV); run 'make user-apps' first" >&2; \
		exit 1; \
	}

check-user-apps-la: user-apps-la
	@test -d "$(USER_BIN_DIR_LA)" || { \
		echo "missing user binaries in $(USER_BIN_DIR_LA); run 'make user-apps-la' first" >&2; \
		exit 1; \
	}

check-rootfs: rootfs
	@test -d "$(ROOTFS_DIR)" || { \
		echo "missing rootfs directory $(ROOTFS_DIR); run 'make all' first" >&2; \
		exit 1; \
	}
	@test -d "$(ROOTFS_DIR)/root" || { \
		echo "rootfs is incomplete under $(ROOTFS_DIR); run 'make all' first" >&2; \
		exit 1; \
	}

check-rootfs-rv: rootfs-rv
	@test -d "$(ROOTFS_RV_DIR)" || { \
		echo "missing rootfs directory $(ROOTFS_RV_DIR); run 'make all' first" >&2; \
		exit 1; \
	}
	@test -d "$(ROOTFS_RV_DIR)/root" || { \
		echo "rootfs is incomplete under $(ROOTFS_RV_DIR); run 'make all' first" >&2; \
		exit 1; \
	}

check-rootfs-la: rootfs-la
	@test -d "$(ROOTFS_LA_DIR)" || { \
		echo "missing rootfs directory $(ROOTFS_LA_DIR); run 'make all' first" >&2; \
		exit 1; \
	}
	@test -d "$(ROOTFS_LA_DIR)/root" || { \
		echo "rootfs is incomplete under $(ROOTFS_LA_DIR); run 'make all' first" >&2; \
		exit 1; \
	}

check-rootfs-rv-ready:
	@test -d "$(ROOTFS_RV_DIR)" || { \
		echo "missing rootfs directory $(ROOTFS_RV_DIR); run 'make rootfs-rv' first" >&2; \
		exit 1; \
	}
	@test -d "$(ROOTFS_RV_DIR)/root" || { \
		echo "rootfs is incomplete under $(ROOTFS_RV_DIR); run 'make rootfs-rv' first" >&2; \
		exit 1; \
	}

check-rootfs-la-ready:
	@test -d "$(ROOTFS_LA_DIR)" || { \
		echo "missing rootfs directory $(ROOTFS_LA_DIR); run 'make rootfs-la' first" >&2; \
		exit 1; \
	}
	@test -d "$(ROOTFS_LA_DIR)/root" || { \
		echo "rootfs is incomplete under $(ROOTFS_LA_DIR); run 'make rootfs-la' first" >&2; \
		exit 1; \
	}

$(DISK_RV_IMG): force check-user-apps-rv rootfs-rv check-rootfs-rv-ready $(OPTIONAL_RUNTIME_FILES) $(ROOTFS_RV_FILES) scripts/pack-disk-img.sh
	MUSL_ARCH=$(RV_MUSL_ARCH) MUSL_LOADER_ALIASES="$(RV_MUSL_LOADER_ALIASES)" ./scripts/pack-disk-img.sh $(ROOTFS_RV_DIR) $(USER_BIN_DIR_RV) $@

$(DISK_LA_IMG): force check-user-apps-la rootfs-la check-rootfs-la-ready $(OPTIONAL_RUNTIME_FILES) $(ROOTFS_LA_FILES) scripts/pack-disk-img.sh
	MUSL_ARCH=$(LA_MUSL_ARCH) MUSL_LOADER_ALIASES="$(LA_MUSL_LOADER_ALIASES)" ./scripts/pack-disk-img.sh $(ROOTFS_LA_DIR) $(USER_BIN_DIR_LA) $@

force:

$(LA_BOOTLOADER_ELF): $(LA_BOOTLOADER_DEPS)
	cd $(LA_BOOTLOADER_DIR) && cargo build --release

prepare-run-test-fs: | $(STAMP_DIR)
	@if [ ! -f "$(TEST_FS)" ]; then \
		echo "Test image not found: $(TEST_FS)"; \
		exit 2; \
	fi
	cp -c "$(TEST_FS)" "$(RUN_TEST_FS)" 2>/dev/null || cp --reflink=auto "$(TEST_FS)" "$(RUN_TEST_FS)" 2>/dev/null || cp "$(TEST_FS)" "$(RUN_TEST_FS)"

prepare-run-test-fs-la: | $(STAMP_DIR)
	@if [ ! -f "$(TEST_FS_LA)" ]; then \
		echo "Test image not found: $(TEST_FS_LA)"; \
		exit 2; \
	fi
	cp -c "$(TEST_FS_LA)" "$(RUN_TEST_FS_LA)" 2>/dev/null || cp --reflink=auto "$(TEST_FS_LA)" "$(RUN_TEST_FS_LA)" 2>/dev/null || cp "$(TEST_FS_LA)" "$(RUN_TEST_FS_LA)"

run: check-kernel $(RUN_DISK_IMG) prepare-run-test-fs
	$(QEMU) -machine virt -kernel $(RUN_KERNEL) -m $(MEM) -nographic -smp $(SMP) -bios default $(QEMU_COMP_BLK_ARGS) -device virtio-net-device,netdev=net -netdev $(QEMU_NETDEV) -no-reboot -rtc base=utc $(QEMU_COMP_EXTRA_BLK_ARGS) $(QEMU_TRACE_ARGS)

run-la: check-kernel-la $(LA_BOOTLOADER_ELF) $(DISK_LA_IMG) prepare-run-test-fs-la
	$(QEMU_LA) -machine virt -cpu la464 -kernel $(LA_BOOTLOADER_ELF) -device loader,file=kernel-la,addr=$(LA_KERNEL_ENTRY_PA) -m $(MEM_LA) -nographic -smp $(SMP) $(QEMU_LA_BLK_ARGS) -device virtio-net-pci,netdev=net0,id=net0 -netdev $(QEMU_LA_NETDEV) -no-reboot -rtc base=utc $(QEMU_LA_EXTRA_BLK_ARGS)

fast-run: check-kernel
	$(QEMU) -machine virt -kernel kernel-rv -m $(MEM) -nographic -smp $(SMP) -bios default $(QEMU_COMP_BLK_ARGS) -device virtio-net-device,netdev=net -netdev $(FAST_RUN_QEMU_NETDEV) -no-reboot -rtc base=utc $(QEMU_COMP_EXTRA_BLK_ARGS) $(QEMU_TRACE_ARGS)

fast-run-la: check-kernel-la $(LA_BOOTLOADER_ELF)
	$(QEMU_LA) -machine virt -cpu la464 -kernel $(LA_BOOTLOADER_ELF) -device loader,file=kernel-la,addr=$(LA_KERNEL_ENTRY_PA) -m $(MEM_LA) -nographic -smp $(SMP) $(QEMU_LA_BLK_ARGS) -device virtio-net-pci,netdev=net0,id=net0 -netdev $(QEMU_LA_NETDEV) -no-reboot -rtc base=utc $(QEMU_LA_EXTRA_BLK_ARGS)

run-trace: QEMU_TRACE_ARGS = -d int,in_asm -D qemu.log
run-trace: run

run-comp-rv:
	$(MAKE) run ARCH=rv

run-comp-la:
	$(MAKE) run ARCH=la

debug: check-kernel $(RUN_DISK_IMG)
	$(MAKE) -C os debug

gdbserver: check-kernel $(RUN_DISK_IMG)
	$(MAKE) -C os gdbserver

gdbclient:
	$(MAKE) -C os gdbclient

docker:
	docker run --network host --rm -it -v ${PWD}:/mnt -w /mnt ${DOCKER_NAME} bash

build_docker:
	docker build -t ${DOCKER_NAME} .

fmt:
	cd fs; cargo fmt; cd ../fs-fuse; cargo fmt; cd ../os; cargo fmt; cd ../user; cargo fmt; cd ..

clean:
	rm -rf $(STAMP_DIR) $(RUN_TEST_FS) $(DISK_RV_IMG) $(DISK_LA_IMG) kernel-rv kernel-la os/.cargo user/.cargo
	$(MAKE) -C os clean
	$(MAKE) -C user clean

clean-all: clean
	rm -rf $(ROOTFS_RV_DIR) $(ROOTFS_LA_DIR) $(ROOTFS_RV_BUILD_DIR) $(ROOTFS_LA_BUILD_DIR) $(ROOTFS_RV_STAMP_DIR) $(ROOTFS_LA_STAMP_DIR)
