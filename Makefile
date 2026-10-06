.PHONY: precommit pre-commit tracked-sources format-check test lint build-release

precommit pre-commit: tracked-sources format-check test lint build-release

tracked-sources:
	@missing="$$(find src tests -type f -print 2>/dev/null | sort | while IFS= read -r path; do \
		git ls-files --error-unmatch -- "$$path" >/dev/null 2>&1 || printf '%s\n' "$$path"; \
	done)"; \
	if test -n "$$missing"; then \
		printf '%s\n' 'Source or test files are not tracked and would be absent from CI:' "$$missing" >&2; \
		exit 1; \
	fi

format-check:
	cargo fmt --all -- --check

test:
	cargo test --locked

lint:
	cargo clippy --locked --all-targets -- -D warnings

build-release:
	cargo build --release --locked
