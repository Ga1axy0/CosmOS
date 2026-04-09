import threading
import socket
import random
import string
import time

def random_text(length=32):
    return ''.join(random.choices(string.ascii_letters + string.digits, k=length))

def tcp_client(thread_id):
    host = '127.0.0.1'
    port = 7777
    while True:
        try:
            with socket.create_connection((host, port), timeout=5) as sock:
                msg = random_text()
                sock.sendall(msg.encode())
                data = sock.recv(1024)
                received_msg = data.decode(errors='ignore')
                print(f"\033[1;32mThread {thread_id}:\033[0m Sent: {msg} | Received: {received_msg}")
                assert msg == received_msg, f"Thread {thread_id}: Mismatch - Sent '{msg}' but received '{received_msg}'"
        except Exception as e:
            print(f"\033[1;31mThread {thread_id}:\033[0m Error - {e}")
        finally:
            time.sleep(0.1)

threads = []
for i in range(8):
    t = threading.Thread(target=tcp_client, args=(i,))
    threads.append(t)
    t.start()
    time.sleep(0.2)

for t in threads:
    t.join()