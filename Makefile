DOCKER_NAME ?= rcore-docker
.PHONY: docker build_docker clean
	
docker:
	docker run --network host --rm -it -v ${PWD}:/mnt -w /mnt ${DOCKER_NAME} bash

build_docker: 
	docker build -t ${DOCKER_NAME} .

fmt:
	cd fs; cargo fmt; cd ../fs-fuse; cargo fmt; cd ../os; cargo fmt; cd ../user; cargo fmt; cd ..

clean:
	$(MAKE) -C os clean
