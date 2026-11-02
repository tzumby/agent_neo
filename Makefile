AWS_PROFILE ?= default
AWS_REGION  ?= ca-central-1
ENV         ?= dev
# Desktops' architecture (Terraform: desktop_arch): amd64 or arm64.
DESKTOP_ARCH ?= amd64
DESKTOP_RUST_TARGET = $(if $(filter arm64,$(DESKTOP_ARCH)),aarch64,x86_64)-unknown-linux-musl
export AWS_PROFILE AWS_REGION

TF_BOOTSTRAP := terraform -chdir=$(CURDIR)/terraform/bootstrap
TF           := terraform -chdir=$(CURDIR)/terraform/envs/$(ENV)

# Instance to target with viewer/shell/logs/soak: the test desktop, or
# DESKTOP=<name>'s (terraform output desktop_instance_ids).
INSTANCE ?= $(if $(DESKTOP),$(shell $(TF) output -json desktop_instance_ids 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin).get("$(DESKTOP)",""))'),$(shell $(TF) output -raw test_desktop_instance_id 2>/dev/null))
# Playbooks run on every running desktop; LIMIT=<instance id> (or DESKTOP=<name>) for one.
# An unknown DESKTOP matches no host rather than all of them.
LIMIT ?= $(if $(DESKTOP),$(or $(INSTANCE),no-desktop-named-$(DESKTOP)),)
PLAYBOOK_LIMIT = $(if $(LIMIT),--limit $(LIMIT),)
SSM_BUCKET = $(shell $(TF) output -raw ssm_transfer_bucket)

.DEFAULT_GOAL := help
.PHONY: help bootstrap init plan apply destroy fmt lint galaxy desktop screenshot llm-keys agentd-test agentd-build agentd-deploy agentd-evals agentd-soak agentd-logs tailnet-build tailnet-deploy tailscale-key tailscale-oauth apns-key gatewayd-test gatewayd-build gateway-image app-tailscalekit app-test app-sim app-device app-mac app-release-mac app-testflight new-desktop cdp viewer shell

help: ## Show targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-14s %s\n", $$1, $$2}'

# --- Terraform ---------------------------------------------------------------

bootstrap: ## Create the S3 state bucket (one time; local state)
	$(TF_BOOTSTRAP) init
	$(TF_BOOTSTRAP) apply

init: ## terraform init the env against the state bucket
	$(TF) init -backend-config="bucket=$$($(TF_BOOTSTRAP) output -raw state_bucket)" -backend-config="region=$(AWS_REGION)"

plan: ## terraform plan the env, saved to tfplan
	$(TF) plan -out=tfplan

apply: ## Apply the saved tfplan
	$(TF) apply tfplan

destroy: ## terraform destroy the env
	$(TF) destroy

fmt: ## Format Terraform code
	terraform fmt -recursive terraform

lint: ## Check formatting, validate Terraform, syntax-check playbooks
	terraform fmt -check -recursive terraform
	$(TF) validate
	cd ansible && ansible-playbook --syntax-check playbooks/desktops.yml playbooks/screenshot.yml playbooks/tailnet-proxy.yml playbooks/agentd.yml

# --- Ansible -----------------------------------------------------------------

# On macOS, forked Ansible workers segfault when botocore looks up the system
# proxy settings (_scproxy). no_proxy='*' skips that lookup.
ANSIBLE_PLAYBOOK := cd ansible && no_proxy='*' OBJC_DISABLE_INITIALIZE_FORK_SAFETY=YES ansible-playbook

galaxy: ## Install the Ansible collections
	cd ansible && ansible-galaxy collection install -r requirements.yml

new-desktop: ## A desktop for someone else, on their own tailnet, step by step: NAME=<name> (docs/onboarding.md)
	scripts/new-desktop.sh $(NAME)

desktop: ## Run the desktop playbook against running desktops (LIMIT= or DESKTOP= for one)
	$(ANSIBLE_PLAYBOOK) playbooks/desktops.yml $(PLAYBOOK_LIMIT) -e ansible_aws_ssm_bucket_name=$(SSM_BUCKET)

screenshot: ## Download a screenshot of each desktop to screenshots/
	$(ANSIBLE_PLAYBOOK) playbooks/screenshot.yml $(PLAYBOOK_LIMIT) -e ansible_aws_ssm_bucket_name=$(SSM_BUCKET)

# --- Agent -------------------------------------------------------------------

llm-keys: ## Store the Together API key in SSM (prompted, not echoed)
	scripts/put-secret.py /cloud-desktops/$(ENV)/llm/together_api_key

# --- agentd and the tailnet ------------------------------------------------------

agentd-test: ## Format check, clippy and unit tests for agentd
	cd services/agentd && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test

agentd-build: ## Build agentd for the desktop (static linux binary for DESKTOP_ARCH)
	cd services/agentd && cargo zigbuild --release --target $(DESKTOP_RUST_TARGET)

agentd-deploy: agentd-build ## Install agentd on running desktops (LIMIT= or DESKTOP= for one)
	$(ANSIBLE_PLAYBOOK) playbooks/agentd.yml $(PLAYBOOK_LIMIT) -e ansible_aws_ssm_bucket_name=$(SSM_BUCKET)

agentd-evals: ## Memory evals on this Mac against Together (cents per scenario): MEMORY_V2=on|off SCENARIO=name
	cd services/agentd && cargo build --quiet
	TOGETHER_API_KEY="$${TOGETHER_API_KEY:-$$(aws ssm get-parameter --name /cloud-desktops/$(ENV)/llm/together_api_key \
	  --with-decryption --query Parameter.Value --output text)}" \
	  services/agentd/evals/run.py --agentd services/agentd/target/debug/agentd \
	  --memory-v2 $(or $(MEMORY_V2),off) $(if $(SCENARIO),--scenario $(SCENARIO))

agentd-soak: ## Memory v2 soak report for the desktop (read-only): SINCE=YYYY-MM-DD, the day the soak started
	scripts/agentd-soak.py $(INSTANCE) $(if $(SINCE),--since $(SINCE))

agentd-logs: ## Show agentd's recent log
	aws ssm start-session --target $(INSTANCE) --document-name AWS-StartInteractiveCommand \
	  --parameters '{"command":["sudo journalctl -u agentd -n 50 --no-pager"]}'

tailnet-build: ## Build tailnet-proxy (tsnet) for the desktop (linux, DESKTOP_ARCH)
	cd services/tailnet-proxy && CGO_ENABLED=0 GOOS=linux GOARCH=$(DESKTOP_ARCH) \
	  go build -trimpath -ldflags "-s -w" -o build/tailnet-proxy-linux-$(DESKTOP_ARCH) .

gatewayd-test: ## Format check, clippy and unit tests for gatewayd (the attested model gateway)
	cd services/gatewayd && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test

gatewayd-build: ## Build gatewayd for the gateway image (static linux/arm64 binary)
	cd services/gatewayd && cargo zigbuild --release --target aarch64-unknown-linux-musl

gateway-image: gatewayd-build ## Build the attested gateway image (AMI) on a temporary builder; prints the AMI and its measurements
	ENV=$(ENV) scripts/build-gateway-image.sh

apns-key: ## Store the APNs push key in SSM: make apns-key FILE=AuthKey_XXXXXXXXXX.p8 KEY_ID=XXXXXXXXXX
	SSM_PREFIX=/cloud-desktops/$(ENV) scripts/put-apns-key.py "$(FILE)" "$(KEY_ID)"

tailscale-key: ## Store a Tailscale auth key (tagged tag:cloud-desktop) in SSM (prompted, not echoed); DESKTOP=<name> for a user's own tailnet
	scripts/put-secret.py /cloud-desktops/$(ENV)/tailscale$(if $(DESKTOP),/users/$(DESKTOP))/auth_key

tailscale-oauth: ## Store the desktops tailnet's OAuth client (id, then secret; prompted, not echoed)
	scripts/put-secret.py /cloud-desktops/$(ENV)/tailscale/desktops/oauth_client_id
	scripts/put-secret.py /cloud-desktops/$(ENV)/tailscale/desktops/oauth_client_secret

tailnet-deploy: tailnet-build ## Put desktops on their tailnets (agent API, noVNC, VNC); deploy agentd first (LIMIT= or DESKTOP= for one)
	$(ANSIBLE_PLAYBOOK) playbooks/tailnet-proxy.yml $(PLAYBOOK_LIMIT) -e ansible_aws_ssm_bucket_name=$(SSM_BUCKET)

# --- Apple apps (apps/apple) -------------------------------------------------------

app-tailscalekit: ## Build TailscaleKit.xcframework from libtailscale (Go + Xcode, a few minutes)
	apps/apple/scripts/build-tailscalekit.sh

app-test: ## Unit tests for DesktopKit (macOS)
	cd apps/apple/DesktopKit && swift test

app-sim: ## Build the iPhone app for the Simulator
	cd apps/apple && xcodegen generate --quiet && xcodebuild -project CloudDesktop.xcodeproj \
	  -scheme CloudDesktop -destination 'generic/platform=iOS Simulator' -derivedDataPath build/sim build -quiet

app-device: ## Build, install and launch the iPhone app on your paired iPhone (unlock it first)
	apps/apple/scripts/run-on-device.sh

app-mac: ## Build the Mac app, install it in /Applications and open it (first run registers this Mac)
	apps/apple/scripts/run-on-mac.sh

app-release-mac: ## The Mac app for others: Developer ID, notarized, in a DMG (docs/onboarding.md)
	apps/apple/scripts/release-mac.sh

app-testflight: ## The iPhone app for TestFlight: archive and upload to App Store Connect (docs/onboarding.md)
	apps/apple/scripts/release-ios.sh

# --- Access (SSM only) -------------------------------------------------------

cdp: ## Forward Chrome's debugging port to localhost:9222 (full control of the logged-in browser while open)
	aws ssm start-session --target $(INSTANCE) --document-name AWS-StartPortForwardingSession \
	  --parameters '{"portNumber":["9222"],"localPortNumber":["9222"]}'

viewer: ## Forward noVNC to http://localhost:6080/vnc.html
	aws ssm start-session --target $(INSTANCE) --document-name AWS-StartPortForwardingSession \
	  --parameters '{"portNumber":["6080"],"localPortNumber":["6080"]}'

shell: ## Open a shell on the desktop over SSM
	aws ssm start-session --target $(INSTANCE)
