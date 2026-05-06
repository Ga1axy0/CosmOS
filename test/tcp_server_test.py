import threading
import socket
import random
import string
import time

MAX_PACKETS = 100
THREAD_COUNT = 10
INTERVAL = 0.01

def random_text(length=32):
    return ''.join(random.choices(string.ascii_letters + string.digits, k=length))

class ThreadStats:
    def __init__(self):
        self.sent = 0
        self.success = 0

def tcp_client(thread_id, stats):
    host = '127.0.0.1'
    port = 7777
    for _ in range(MAX_PACKETS):
        try:
            with socket.create_connection((host, port), timeout=5) as sock:
                msg = random_text(length=256)
                sock.sendall(msg.encode())
                data = sock.recv(1024)
                received_msg = data.decode(errors='ignore')
                stats.sent += 1
                if msg == received_msg:
                    stats.success += 1
                print(f"\033[1;32mThread {thread_id}:\033[0m Sent: {msg[:16]} | Received: {received_msg[:16]} | {'Match' if msg == received_msg else 'Mismatch'}")
                assert msg == received_msg, f"Thread {thread_id}: Mismatch - Sent '{msg}' but received '{received_msg}'"
        except Exception as e:
            print(f"\033[1;31mThread {thread_id}:\033[0m Error - {e}")
        finally:
            time.sleep(INTERVAL)

threads = []
stats_list = [ThreadStats() for _ in range(THREAD_COUNT)]

start_time = time.time()

for i in range(THREAD_COUNT):
    t = threading.Thread(target=tcp_client, args=(i, stats_list[i]))
    threads.append(t)
    t.start()
    time.sleep(INTERVAL)

for t in threads:
    t.join()

end_time = time.time()
print(f"\nTest completed in {end_time - start_time:.2f} seconds.")

total_sent = sum(s.sent for s in stats_list)
total_success = sum(s.success for s in stats_list)
success_rate = (total_success / total_sent) * 100 if total_sent else 0

print(f"\nTotal packets sent: {total_sent}")
print(f"Total packets matched: {total_success}")
print(f"Success rate: {success_rate:.2f}%")
print(f"Average packet per second: {total_sent / (end_time - start_time):.2f}")