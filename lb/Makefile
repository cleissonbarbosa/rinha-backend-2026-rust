CC ?= gcc
CFLAGS ?= -O3 -pipe -Wall -Wextra -Werror -std=c11

.PHONY: all clean

all: lb

lb: lb.c
	$(CC) $(CFLAGS) -o $@ $<

clean:
	rm -f lb
