# llama2.c / stories15M BAIS workload

This directory vendors run.c and the MIT license from upstream
[karpathy/llama2.c](https://github.com/karpathy/llama2.c) commit
350e04fe35433e6d2941dce5a1f53308f87058eb.

The source keeps the upstream model and tokenizer implementation. CosmOS adds:

- explicit stdint.h inclusion for musl;
- OpenMP worker configuration;
- token/layer/arrival hints to syscall 480;
- per-token and end-to-end latency output;
- an optional, separately forked write-and-fsync workload representing non-AI
  filesystem and VirtIO completion work.

The model is not committed. Run:

    ./scripts/prepare-llama2c-model.sh
    ./scripts/build-llama2c-bais-rv.sh

The download script pins and checks:

- stories15M.bin SHA-256:
  cd590644d963867a2b6e5a1107f51fad663c41d79c149fbecbbb1f95fa81f49a
- tokenizer.bin SHA-256:
  50a52ef822ee9e83de5ce9d0be0a025a773d019437f58b5ff9dcafb063ece361

For a Linux/qemu-user correctness smoke test (no CosmOS BAIS syscall):

    QEMU_LD_PREFIX=CosmOS-rootfs/rootfs-rv \
    qemu-riscv64 benchmarks/llama2c/build/llama2c-bais-rv \
      benchmarks/llama2c/cache/stories15M.bin \
      -z benchmarks/llama2c/cache/tokenizer.bin -t 0 -n 8 \
      -i 'Once upon a time' -c 8 -b none

off is the instrumented scheduler baseline. none disables both the CosmOS
control operation and hints and is only meant for portability checks.
