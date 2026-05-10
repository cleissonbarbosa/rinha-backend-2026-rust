API_MANIFEST=api/Cargo.toml

.PHONY: build api lb bench analyze docker-build up down clean test smoke-test

build: api lb

api:
	cargo build --manifest-path $(API_MANIFEST) --release --bin api

lb:
	$(MAKE) -C lb

docker/build:
	docker compose build

up:
	docker compose up --build -d

down:
	docker compose down --remove-orphans

clean:
	cargo clean --manifest-path $(API_MANIFEST)
	$(MAKE) -C lb clean

test:
	sh ./scripts/official-test.sh full

smoke-test:
	sh ./scripts/official-test.sh smoke

docker/push:
	docker compose push