DOCKER_NAME ?= rcore-docker

USER_MODE ?= release
RV_USER_TARGET := riscv64gc-unknown-none-elf
LA_USER_TARGET := loongarch64-unknown-none
RV_KERNEL_TARGET := riscv64gc-unknown-none-elf
LA_KERNEL_TARGET := loongarch64-unknown-none
USER_BIN_DIR_RV := user/target/$(RV_USER_TARGET)/$(USER_MODE)
USER_BIN_DIR_LA := user/target/$(LA_USER_TARGET)/$(USER_MODE)
USER_BIN_DIR ?= $(USER_BIN_DIR_RV)
KERNEL_RV_ELF := os/target/$(RV_KERNEL_TARGET)/release/os
KERNEL_LA_ELF := os/target/$(LA_KERNEL_TARGET)/release/os
QEMU ?= qemu-system-riscv64
MEM ?= 1G
SMP ?= 1
TEST_FS ?= sdcard-rv.img
# make run 使用写时复制副本，避免 QEMU 写坏原始测试镜像。
RUN_TEST_FS ?= .make/sdcard-rv-run.img
QEMU_NETDEV ?= user,id=net
QEMU_TRACE_ARGS ?=
QEMU_COMP_BLK_ARGS = -drive file=$(RUN_TEST_FS),if=none,format=raw,id=x0 -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0
QEMU_COMP_EXTRA_BLK_ARGS = -drive file=disk.img,if=none,format=raw,id=x1 -device virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1
QEMU_LA ?= qemu-system-loongarch64
MEM_LA ?= 4G
TEST_FS_LA ?= sdcard-la.img
RUN_TEST_FS_LA ?= .make/sdcard-la-run.img
QEMU_LA_NETDEV ?= user,id=net0
LA_KERNEL_ENTRY_PA ?= 0x90000000
LA_BOOTLOADER_DIR ?= bootloader/loongarch64-direct
LA_BOOTLOADER_ELF := $(LA_BOOTLOADER_DIR)/target/loongarch64-unknown-none/release/loongarch64-direct-boot

STAMP_DIR := .make
USER_BUILD_STAMP_RV := $(USER_BIN_DIR_RV)/.xxos-build.stamp
USER_BUILD_STAMP_LA := $(USER_BIN_DIR_LA)/.xxos-build.stamp
KERNEL_BUILD_STAMP_RV := $(STAMP_DIR)/kernel-build-rv.stamp
KERNEL_BUILD_STAMP_LA := $(STAMP_DIR)/kernel-build-la.stamp
USER_BUILD_DEPS := user/Makefile user/Cargo.toml $(shell find user/src -type f | sort)
KERNEL_BUILD_DEPS := os/Makefile os/Cargo.toml os/build.rs $(shell find os/src fs/src -type f | sort)
LA_BOOTLOADER_DEPS := $(LA_BOOTLOADER_DIR)/Cargo.toml $(LA_BOOTLOADER_DIR)/Cargo.lock $(LA_BOOTLOADER_DIR)/build.rs $(LA_BOOTLOADER_DIR)/linker.ld $(shell find $(LA_BOOTLOADER_DIR)/src -type f | sort)
ROOTFS_REPO := CosmOS-rootfs
ROOTFS_SRC_DIR := $(ROOTFS_REPO)
ROOTFS_DIR := $(STAMP_DIR)/rootfs
ROOTFS_RV_DIR := $(STAMP_DIR)/rootfs-rv
ROOTFS_LA_DIR := $(STAMP_DIR)/rootfs-la
ROOTFS_STAMP := $(STAMP_DIR)/rootfs.stamp
ROOTFS_RV_STAMP := $(STAMP_DIR)/rootfs-rv.stamp
ROOTFS_LA_STAMP := $(STAMP_DIR)/rootfs-la.stamp
ROOTFS_SRC_FILES := $(shell if [ -d $(ROOTFS_SRC_DIR)/rootfs ]; then find $(ROOTFS_SRC_DIR)/rootfs -type f | sort; fi)
ROOTFS_RV_FILES := $(ROOTFS_SRC_FILES)
ROOTFS_LA_FILES := $(ROOTFS_SRC_FILES)
DISK_RV_IMG := disk.img
DISK_LA_IMG := disk-la.img
QEMU_LA_BLK_ARGS = -drive file=$(RUN_TEST_FS_LA),if=none,format=raw,id=x0 -device virtio-blk-pci,drive=x0,id=x0
QEMU_LA_EXTRA_BLK_ARGS = -drive file=$(DISK_LA_IMG),if=none,format=raw,id=x1 -device virtio-blk-pci,drive=x1,id=x1
RV_ROOTFS_TARGET ?= riscv64-linux-musl
RV_TOOLCHAIN_BIN ?= /opt/riscv64-linux-musl-cross/bin
RV_GLIBC_LIB ?= /usr/riscv64-linux-gnu/lib
RV_MUSL_LIB ?= /opt/riscv64-linux-musl-cross/riscv64-linux-musl/lib
RV_MUSL_ARCH ?= riscv64
LA_ROOTFS_TARGET ?= loongarch64-linux-musl
LA_TOOLCHAIN_BIN ?= /opt/loongarch64-linux-musl-cross/bin
LA_GLIBC_TOOLCHAIN ?= /opt/gcc-13.2.0-loongarch64-linux-gnu
LA_MUSL_LIB ?= /opt/loongarch64-linux-musl-cross/loongarch64-linux-musl/lib
LA_MUSL_ARCH ?= loongarch64
OPTIONAL_RUNTIME_FILES := $(wildcard lib/musl/ar lib/glibc/ar)

.PHONY: all submodules cargo-config docker build_docker fmt user-apps user-apps-la rootfs rootfs-rv rootfs-la rv la disk-la clean run run-la fast-run fast-run-la run-trace run-comp-rv debug gdbserver gdbclient check-kernel check-kernel-la check-user-apps-rv check-user-apps-la check-rootfs check-rootfs-rv check-rootfs-la prepare-run-test-fs prepare-run-test-fs-la check-run-test-fs check-run-test-fs-la

all:
	$(MAKE) submodules
	$(MAKE) cargo-config
	$(MAKE) user-apps user-apps-la kernel-rv kernel-la $(DISK_RV_IMG) $(DISK_LA_IMG)

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

$(KERNEL_BUILD_STAMP_RV): $(KERNEL_BUILD_DEPS) | $(STAMP_DIR)
	$(MAKE) -C os kernel ARCH=riscv64
	touch $@

$(KERNEL_BUILD_STAMP_LA): $(KERNEL_BUILD_DEPS) | $(STAMP_DIR)
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

rootfs: $(ROOTFS_STAMP)

$(ROOTFS_RV_STAMP): $(ROOTFS_SRC_FILES) | $(STAMP_DIR)
	rm -rf $(ROOTFS_RV_DIR)
	cp -a $(ROOTFS_SRC_DIR)/rootfs/. $(ROOTFS_RV_DIR)/
	touch $@

rootfs-rv: $(ROOTFS_RV_STAMP)

$(ROOTFS_LA_STAMP): $(ROOTFS_SRC_FILES) | $(STAMP_DIR)
	rm -rf $(ROOTFS_LA_DIR)
	cp -a $(ROOTFS_SRC_DIR)/rootfs/. $(ROOTFS_LA_DIR)/
	touch $@

rootfs-la: $(ROOTFS_LA_STAMP)

rv: $(DISK_RV_IMG)

la disk-la: $(DISK_LA_IMG)

check-kernel:
	@test -x kernel-rv || { \
		echo "missing kernel-rv; run 'make all' first" >&2; \
		exit 1; \
	}

check-kernel-la:
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

$(DISK_RV_IMG): USER_BIN_DIR := $(USER_BIN_DIR_RV)
$(DISK_RV_IMG): check-user-apps-rv check-rootfs-rv $(OPTIONAL_RUNTIME_FILES) $(ROOTFS_RV_FILES) scripts/pack-disk-img.sh
	MUSL_ARCH=$(RV_MUSL_ARCH) ./scripts/pack-disk-img.sh $(ROOTFS_RV_DIR) $(USER_BIN_DIR) $@

$(DISK_LA_IMG): USER_BIN_DIR := $(USER_BIN_DIR_LA)
$(DISK_LA_IMG): check-user-apps-la check-rootfs-la $(OPTIONAL_RUNTIME_FILES) $(ROOTFS_LA_FILES) scripts/pack-disk-img.sh
	MUSL_ARCH=$(LA_MUSL_ARCH) ./scripts/pack-disk-img.sh $(ROOTFS_LA_DIR) $(USER_BIN_DIR) $@

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

check-run-test-fs:
	@if [ ! -f "$(RUN_TEST_FS)" ]; then \
		echo "Run image not found: $(RUN_TEST_FS). Run 'make run' once first."; \
		exit 2; \
	fi

check-run-test-fs-la:
	@if [ ! -f "$(RUN_TEST_FS_LA)" ]; then \
		echo "Run image not found: $(RUN_TEST_FS_LA). Run 'make run-la' once first."; \
		exit 2; \
	fi

run: check-kernel disk.img prepare-run-test-fs
	$(QEMU) -machine virt -kernel kernel-rv -m $(MEM) -nographic -smp $(SMP) -bios default $(QEMU_COMP_BLK_ARGS) -device virtio-net-device,netdev=net -netdev $(QEMU_NETDEV) -no-reboot -rtc base=utc $(QEMU_COMP_EXTRA_BLK_ARGS) $(QEMU_TRACE_ARGS)

run-la: check-kernel-la $(LA_BOOTLOADER_ELF) $(DISK_LA_IMG) prepare-run-test-fs-la
	$(QEMU_LA) -machine virt -cpu la464 -kernel $(LA_BOOTLOADER_ELF) -device loader,file=kernel-la,addr=$(LA_KERNEL_ENTRY_PA) -m $(MEM_LA) -nographic -smp $(SMP) $(QEMU_LA_BLK_ARGS) -device virtio-net-pci,netdev=net0,id=net0 -netdev $(QEMU_LA_NETDEV) -no-reboot -rtc base=utc $(QEMU_LA_EXTRA_BLK_ARGS)

fast-run: check-kernel disk.img check-run-test-fs
	$(QEMU) -machine virt -kernel kernel-rv -m $(MEM) -nographic -smp $(SMP) -bios default $(QEMU_COMP_BLK_ARGS) -device virtio-net-device,netdev=net -netdev $(QEMU_NETDEV) -no-reboot -rtc base=utc $(QEMU_COMP_EXTRA_BLK_ARGS) $(QEMU_TRACE_ARGS)

fast-run-la: check-kernel-la $(LA_BOOTLOADER_ELF) $(DISK_LA_IMG) check-run-test-fs-la
	$(QEMU_LA) -machine virt -cpu la464 -kernel $(LA_BOOTLOADER_ELF) -device loader,file=kernel-la,addr=$(LA_KERNEL_ENTRY_PA) -m $(MEM_LA) -nographic -smp $(SMP) $(QEMU_LA_BLK_ARGS) -device virtio-net-pci,netdev=net0,id=net0 -netdev $(QEMU_LA_NETDEV) -no-reboot -rtc base=utc $(QEMU_LA_EXTRA_BLK_ARGS)

run-trace: QEMU_TRACE_ARGS = -d int,in_asm -D qemu.log
run-trace: run

run-comp-rv: run

debug: check-kernel disk.img
	$(MAKE) -C os debug

gdbserver: check-kernel disk.img
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
	rm -rf $(STAMP_DIR) $(RUN_TEST_FS) $(RUN_TEST_FS_LA) $(DISK_RV_IMG) $(DISK_LA_IMG) kernel-rv kernel-la os/.cargo user/.cargo $(ROOTFS_RV_DIR) $(ROOTFS_LA_DIR) $(ROOTFS_RV_STAMP_DIR) $(ROOTFS_LA_STAMP_DIR)
	$(MAKE) -C os clean
	$(MAKE) -C user clean
