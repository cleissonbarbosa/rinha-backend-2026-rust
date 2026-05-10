CC ?= gcc
CFLAGS ?= -O3 -pipe -Wall -Wextra -Werror -std=c11 -march=x86-64-v3 -flto=auto

.PHONY: all clean

all: lb

lb: lb.c Makefile
	$(CC) $(CFLAGS) -o $@ $<

clean:
	rm -f lb
