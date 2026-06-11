DOCKER_NAME ?= rcore-docker

TARGET ?= riscv64gc-unknown-none-elf
USER_MODE ?= release
USER_BIN_DIR := user/target/$(TARGET)/$(USER_MODE)
KERNEL_RV_ELF := os/target/$(TARGET)/release/os
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

STAMP_DIR := .make
USER_BUILD_STAMP := $(STAMP_DIR)/user-build.stamp
KERNEL_BUILD_STAMP := $(STAMP_DIR)/kernel-build.stamp
USER_BUILD_DEPS := user/Makefile user/Cargo.toml $(shell find user/src -type f | sort)
KERNEL_BUILD_DEPS := os/Makefile os/Cargo.toml os/build.rs $(shell find os/src fs/src -type f | sort)
ROOTFS_DIR := rootfs
ROOTFS_TAR := rootfs.tar
ROOTFS_FILES := $(shell if [ -d $(ROOTFS_DIR) ]; then find $(ROOTFS_DIR) -type f | sort; fi) $(wildcard $(ROOTFS_TAR))
OPTIONAL_RUNTIME_FILES := $(wildcard lib/musl/ar lib/glibc/ar)

.PHONY: all submodules cargo-config docker build_docker fmt user-apps rootfs clean run run-trace run-comp-rv debug gdbserver gdbclient check-kernel check-user-apps check-rootfs prepare-rootfs prepare-run-test-fs

all:
	$(MAKE) submodules
	$(MAKE) cargo-config
	$(MAKE) user-apps kernel-rv kernel-la disk.img

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

$(USER_BUILD_STAMP): $(USER_BUILD_DEPS) | $(STAMP_DIR) cargo-config
	$(MAKE) -C user build
	touch $@

user-apps: $(USER_BUILD_STAMP)

$(KERNEL_BUILD_STAMP): $(KERNEL_BUILD_DEPS) | $(STAMP_DIR) cargo-config
	$(MAKE) -C os kernel
	touch $@

kernel-rv: $(KERNEL_BUILD_STAMP)
	cp $(KERNEL_RV_ELF) $@

kernel-la: kernel-rv
	@echo "warning: LoongArch kernel is not implemented in this repository yet; using kernel-rv as a temporary placeholder." >&2
	cp kernel-rv $@

rootfs:
	@test -d "$(ROOTFS_DIR)" || { \
		echo "missing rootfs directory $(ROOTFS_DIR); run 'make submodules' first" >&2; \
		exit 1; \
	}

check-kernel:
	@test -x kernel-rv || { \
		echo "missing kernel-rv; run 'make all' first" >&2; \
		exit 1; \
	}

check-user-apps:
	@test -d "$(USER_BIN_DIR)" || { \
		echo "missing user binaries in $(USER_BIN_DIR); run 'make all' first" >&2; \
		exit 1; \
	}

check-rootfs: prepare-rootfs
	@test -d "$(ROOTFS_DIR)" || { \
		echo "missing rootfs directory $(ROOTFS_DIR); run 'make all' first" >&2; \
		exit 1; \
	}
	@test -d "$(ROOTFS_DIR)/root" || { \
		echo "rootfs is incomplete under $(ROOTFS_DIR); run 'make all' first" >&2; \
		exit 1; \
	}

# GitLab 快照用 rootfs.tar 代替 rootfs 子模块，构建前先恢复目录。
prepare-rootfs:
	@if [ -f "$(ROOTFS_TAR)" ]; then \
		echo "unpacking $(ROOTFS_TAR) into $(ROOTFS_DIR)"; \
		rm -rf "$(ROOTFS_DIR)"; \
		mkdir -p "$(ROOTFS_DIR)"; \
		tar -xf "$(ROOTFS_TAR)" -C "$(ROOTFS_DIR)"; \
	fi

disk.img: check-user-apps check-rootfs $(OPTIONAL_RUNTIME_FILES) $(ROOTFS_FILES) scripts/pack-disk-img.sh
	./scripts/pack-disk-img.sh $(ROOTFS_DIR) $(USER_BIN_DIR) $@

prepare-run-test-fs: | $(STAMP_DIR)
	@if [ ! -f "$(TEST_FS)" ]; then \
		echo "Test image not found: $(TEST_FS)"; \
		exit 2; \
	fi
	cp -c "$(TEST_FS)" "$(RUN_TEST_FS)" 2>/dev/null || cp --reflink=auto "$(TEST_FS)" "$(RUN_TEST_FS)" 2>/dev/null || cp "$(TEST_FS)" "$(RUN_TEST_FS)"

run: check-kernel disk.img prepare-run-test-fs
	$(QEMU) -machine virt -kernel kernel-rv -m $(MEM) -nographic -smp $(SMP) -bios default $(QEMU_COMP_BLK_ARGS) -device virtio-net-device,netdev=net -netdev $(QEMU_NETDEV) -no-reboot -rtc base=utc $(QEMU_COMP_EXTRA_BLK_ARGS) $(QEMU_TRACE_ARGS)

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
	rm -rf $(STAMP_DIR) $(RUN_TEST_FS) disk.img kernel-rv kernel-la os/.cargo user/.cargo
	$(MAKE) -C os clean
	$(MAKE) -C user clean
