use alloc::collections::vec_deque::VecDeque;
use core::marker::PhantomData;

use bitflags::bitflags;

use crate::poll::{notify_poll_source, POLLIN};
use crate::sync::SpinNoIrqLock;
use crate::task::WaitQueue;
#[cfg(target_arch = "loongarch64")]
use crate::task::yield_current_and_run_next;
#[cfg(not(target_arch = "loongarch64"))]
use crate::task::WaitReason;

use super::{set_uart_ready, CharDevice};

bitflags! {
   struct IER: u8 {
      const RX_AVAILABLE = 1 << 0;
   }
}

bitflags! {
   struct MCR: u8 {
      const DATA_TERMINAL_READY = 1 << 0;
      const REQUEST_TO_SEND     = 1 << 1;
      const AUX_OUTPUT2         = 1 << 3;
   }
}

bitflags! {
   struct LSR: u8 {
      const DATA_READY = 1 << 0;
      const THR_EMPTY  = 1 << 5;
   }
}

const REG_RBR_THR_DLL: usize = 0x00;
const REG_IER_DLM: usize = 0x01;
const REG_MCR: usize = 0x04;
const REG_LSR: usize = 0x05;

#[derive(Copy, Clone)]
struct Mmio<T> {
   addr: *mut T,
   _pd: PhantomData<T>,
}

impl<T> Mmio<T> {
   const fn new(addr: usize) -> Self {
      Self {
         addr: addr as *mut T,
         _pd: PhantomData,
      }
   }
}

impl<T: Copy> Mmio<T> {
   fn read(&self) -> T {
      unsafe { core::ptr::read_volatile(self.addr) }
   }
}

impl<T> Mmio<T> {
   fn write(&self, value: T) {
      unsafe { core::ptr::write_volatile(self.addr, value) }
   }
}

#[derive(Copy, Clone)]
struct Reg8(Mmio<u8>);

impl Reg8 {
   const fn new(addr: usize) -> Self {
      Self(Mmio::new(addr))
   }

   fn read(&self) -> u8 {
      self.0.read()
   }

   fn write(&self, value: u8) {
      self.0.write(value)
   }
}

#[derive(Copy, Clone)]
struct IERReg(Reg8);

impl IERReg {
   const fn new(addr: usize) -> Self {
      Self(Reg8::new(addr))
   }

   fn write(&self, value: IER) {
      self.0.write(value.bits());
   }
}

#[derive(Copy, Clone)]
struct MCRReg(Reg8);

impl MCRReg {
   const fn new(addr: usize) -> Self {
      Self(Reg8::new(addr))
   }

   fn write(&self, value: MCR) {
      self.0.write(value.bits());
   }
}

#[derive(Copy, Clone)]
struct LSRReg(Reg8);

impl LSRReg {
   const fn new(addr: usize) -> Self {
      Self(Reg8::new(addr))
   }

   fn read(&self) -> LSR {
      LSR::from_bits_truncate(self.0.read())
   }
}

#[derive(Copy, Clone)]
struct ReadEnd {
   rbr: Reg8,
   ier: IERReg,
   mcr: MCRReg,
   lsr: LSRReg,
}

#[derive(Copy, Clone)]
struct WriteEnd {
   thr: Reg8,
   lsr: LSRReg,
}

/// NS16550a char device.
pub struct NS16550a<const BASE_ADDR: usize> {
   pub(crate) inner: SpinNoIrqLock<NS16550aInner>,
   #[allow(dead_code)]
   pub(crate) rx_wait_queue: WaitQueue,
}

pub(crate) struct NS16550aInner {
   ns16550a: NS16550aRaw,
   #[allow(dead_code)]
   read_buffer: VecDeque<u8>,
}

pub(crate) struct NS16550aRaw {
   base_addr: usize,
}

impl NS16550aRaw {
   pub const fn new(base_addr: usize) -> Self {
      Self { base_addr }
   }

   fn read_end(&self) -> ReadEnd {
      ReadEnd {
         rbr: Reg8::new(self.base_addr + REG_RBR_THR_DLL),
         ier: IERReg::new(self.base_addr + REG_IER_DLM),
         mcr: MCRReg::new(self.base_addr + REG_MCR),
         lsr: LSRReg::new(self.base_addr + REG_LSR),
      }
   }

   fn write_end(&self) -> WriteEnd {
      WriteEnd {
         thr: Reg8::new(self.base_addr + REG_RBR_THR_DLL),
         lsr: LSRReg::new(self.base_addr + REG_LSR),
      }
   }

   fn try_read(&mut self) -> Option<u8> {
      let read_end = self.read_end();
      if read_end.lsr.read().contains(LSR::DATA_READY) {
         Some(read_end.rbr.read())
      } else {
         None
      }
   }

   fn has_data(&self) -> bool {
      self.read_end().lsr.read().contains(LSR::DATA_READY)
   }
}

impl<const BASE_ADDR: usize> NS16550a<BASE_ADDR> {
   /// new device
   pub fn new() -> Self {
      let mut inner = NS16550aInner {
            ns16550a: NS16550aRaw::new(BASE_ADDR),
            read_buffer: VecDeque::new(),
      };
      inner.ns16550a.init();
      let device = Self {
            inner: unsafe { SpinNoIrqLock::new(inner) },
         rx_wait_queue: WaitQueue::new(),
      };
      set_uart_ready();
      device
   }
}

impl NS16550aRaw {
   pub fn init(&mut self) {
      let read_end = self.read_end();
      let mut mcr = MCR::empty();
      mcr |= MCR::DATA_TERMINAL_READY;
      mcr |= MCR::REQUEST_TO_SEND;
      mcr |= MCR::AUX_OUTPUT2;
      read_end.mcr.write(mcr);
      let ier = IER::RX_AVAILABLE;
      read_end.ier.write(ier);
   }
}

impl<const BASE_ADDR: usize> CharDevice for NS16550a<BASE_ADDR> {
   fn write(&self, ch: u8) {
      let mut inner = self.inner.lock();
      inner.ns16550a.write(ch);
   }

   fn read(&self) -> u8 {
      loop {
         let mut inner = self.inner.lock();
         if let Some(ch) = inner.read_buffer.pop_front() {
            return ch;
         }
         if let Some(ch) = inner.ns16550a.try_read() {
            return ch;
         }
         drop(inner);

         #[cfg(target_arch = "loongarch64")]
         {
            // LA64 bring-up does not have UART RX interrupts wired through a
            // platform IRQ controller yet, so avoid sleeping forever waiting
            // for an IRQ wakeup that never comes. Poll cooperatively instead.
            yield_current_and_run_next();
         }

         #[cfg(not(target_arch = "loongarch64"))]
         {
            // No data: block current task until UART IRQ pushes data and signals.
            self.rx_wait_queue
               .wait_with_reason_or_skip(WaitReason::UartRx, || {
                  let mut inner = self.inner.lock();
                  if !inner.read_buffer.is_empty() {
                     return true;
                  }
                  if let Some(ch) = inner.ns16550a.try_read() {
                     inner.read_buffer.push_back(ch);
                     return true;
                  }
                  false
               });
         }
      }
   }

   fn read_nonblocking(&self) -> Option<u8> {
      let mut inner = self.inner.lock();
      if let Some(ch) = inner.read_buffer.pop_front() {
         return Some(ch);
      }
      inner.ns16550a.try_read()
   }

   fn has_data(&self) -> bool {
      let inner = self.inner.lock();
      !inner.read_buffer.is_empty() || inner.ns16550a.has_data()
   }

   fn handle_irq(&self) {
      let mut inner = self.inner.lock();
      let mut pushed = false;
      while let Some(ch) = inner.ns16550a.try_read() {
         inner.read_buffer.push_back(ch);
         pushed = true;
      }
      drop(inner);
      if pushed {
         self.rx_wait_queue.wake_one();
         notify_poll_source(self as *const Self as usize, POLLIN);
      }
   }
}

impl NS16550aRaw {
   pub fn write(&mut self, ch: u8) {
      let write_end = self.write_end();
      loop {
            if write_end.lsr.read().contains(LSR::THR_EMPTY) {
               write_end.thr.write(ch);
               break;
            }
      }
   }
}
