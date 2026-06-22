# Pino build / publish rules.
#
#   make push           # multi-arch buildx build + push to Docker Hub
#   make build          # build for the local arch, load into the docker engine
#   make push TAG=v0.2  # override the tag
#
# IMAGE is the Docker Hub repo the Sandboxes kit (sbx-kit/) pulls the
# pino-proxy binary out of. Keep IMAGE in sync with PINO_IMAGE in sbx-kit/spec.yaml.

IMAGE     ?= docker.io/longnguyen58445/pino
TAG       ?= sbx
PLATFORMS ?= linux/amd64,linux/arm64
BUILDER   ?= pino-builder

.PHONY: push build builder login

## push: multi-arch build and push to $(IMAGE):$(TAG)
push: builder
	docker buildx build \
		--builder $(BUILDER) \
		--platform $(PLATFORMS) \
		--tag $(IMAGE):$(TAG) \
		--push \
		.
	@echo "pushed $(IMAGE):$(TAG) ($(PLATFORMS))"

## build: build for the local arch only and load into the local docker engine
build:
	docker buildx build \
		--tag $(IMAGE):$(TAG) \
		--load \
		.
	@echo "built $(IMAGE):$(TAG) (local arch, loaded)"

## builder: ensure a buildx builder capable of multi-arch exists
builder:
	@docker buildx inspect $(BUILDER) >/dev/null 2>&1 \
		|| docker buildx create --name $(BUILDER) --driver docker-container --bootstrap

## login: log in to Docker Hub (prompts for credentials)
login:
	docker login docker.io
