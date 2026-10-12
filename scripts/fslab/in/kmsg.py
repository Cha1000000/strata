# SPDX-License-Identifier: MIT
"""Reads the kernel log through /dev/kmsg, as dmesg does.

kmsg.py last         prints the sequence number of the newest record
kmsg.py since SEQ    prints the cifs messages logged after record SEQ
"""
import os
import sys


def records():
    fd = os.open("/dev/kmsg", os.O_RDONLY | os.O_NONBLOCK)
    try:
        while True:
            try:
                record = os.read(fd, 8192)
            except BlockingIOError:
                return
            except BrokenPipeError:
                # Records were overwritten while reading; the next read resumes.
                continue
            head, _, text = record.decode(errors="replace").partition(";")
            yield int(head.split(",")[1]), text.split("\n", 1)[0]
    finally:
        os.close(fd)


def main():
    if sys.argv[1:] == ["last"]:
        print(max((seq for seq, _ in records()), default=-1))
    elif len(sys.argv) == 3 and sys.argv[1] == "since":
        start = int(sys.argv[2])
        for seq, text in records():
            if seq > start and "cifs" in text.lower():
                print(text)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
