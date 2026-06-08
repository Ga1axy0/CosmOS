DOCKER_NAME ?= rcore-docker

TARGET ?= riscv64gc-unknown-none-elf
USER_MODE ?= release
USER_BIN_DIR := user/target/$(TARGET)/$(USER_MODE)
KERNEL_RV_ELF := os/target/$(TARGET)/release/os
QEMU ?= qemu-system-riscv64
MEM ?= 2G
SMP ?= 8
TEST_FS ?= sdcard-rv.img
QEMU_NETDEV ?= user,id=net
QEMU_TRACE_ARGS ?=
QEMU_COMP_BLK_ARGS = -drive file=$(TEST_FS),if=none,format=raw,id=x0 -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0
QEMU_COMP_EXTRA_BLK_ARGS = -drive file=disk.img,if=none,format=raw,id=x1 -device virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1

STAMP_DIR := .make
USER_BUILD_STAMP := $(STAMP_DIR)/user-build.stamp
KERNEL_BUILD_STAMP := $(STAMP_DIR)/kernel-build.stamp
USER_BUILD_DEPS := user/Makefile user/Cargo.toml $(shell find user/src -type f | sort)
KERNEL_BUILD_DEPS := os/Makefile os/Cargo.toml os/build.rs $(shell find os/src fs/src -type f | sort)
ROOTFS_FILES := $(shell find rootfs -type f | sort)
OPTIONAL_RUNTIME_FILES := $(wildcard lib/musl/ar lib/glibc/ar)

.PHONY: all docker build_docker fmt user-apps clean run run-trace run-comp-rv debug gdbserver gdbclient

all: kernel-rv kernel-la disk.img

$(STAMP_DIR):
	mkdir -p $@

$(USER_BUILD_STAMP): $(USER_BUILD_DEPS) | $(STAMP_DIR)
	$(MAKE) -C user build
	touch $@

user-apps: $(USER_BUILD_STAMP)

$(KERNEL_BUILD_STAMP): $(KERNEL_BUILD_DEPS) | $(STAMP_DIR)
	$(MAKE) -C os kernel
	touch $@

kernel-rv: $(KERNEL_BUILD_STAMP)
	cp $(KERNEL_RV_ELF) $@

kernel-la: kernel-rv
	@echo "warning: LoongArch kernel is not implemented in this repository yet; using kernel-rv as a temporary placeholder." >&2
	cp kernel-rv $@

disk.img: $(USER_BUILD_STAMP) $(ROOTFS_FILES) $(OPTIONAL_RUNTIME_FILES) scripts/pack-disk-img.sh
	./scripts/pack-disk-img.sh rootfs $(USER_BIN_DIR) $@

run: kernel-rv disk.img
	$(QEMU) -machine virt -kernel kernel-rv -m $(MEM) -nographic -smp $(SMP) -bios default $(QEMU_COMP_BLK_ARGS) -device virtio-net-device,netdev=net -netdev $(QEMU_NETDEV) -no-reboot -rtc base=utc $(QEMU_COMP_EXTRA_BLK_ARGS) $(QEMU_TRACE_ARGS)

run-trace: QEMU_TRACE_ARGS = -d int,in_asm -D qemu.log
run-trace: run

run-comp-rv: run

debug: kernel-rv disk.img
	$(MAKE) -C os debug

gdbserver: kernel-rv disk.img
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
	rm -rf $(STAMP_DIR) disk.img kernel-rv kernel-la
	$(MAKE) -C os clean
	$(MAKE) -C user clean
