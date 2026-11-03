AWS_PROFILE ?= default
AWS_REGION  ?= ca-central-1
ENV         ?= dev
# Desktops' architecture (Terraform: desktop_arch): amd64 or arm64.
DESKTOP_ARCH ?= amd64
DESKTOP_RUST_TARGET = $(if $(filter arm64,$(DESKTOP_ARCH)),aarch64,x86_64)-unknown-linux-musl
export AWS_PROFILE AWS_REGION

TF_BOOTSTRAP := terraform -chdir=$(CURDIR)/terraform/bootstrap
TF           := terraform -chdir=$(CURDIR)/terraform/envs/$(ENV)

# Any other ENV is a customer's deployment, in their sandbox account
# (docs/customer-cloud.md): its state bucket is in that account (the bootstrap
# stack's workspace of the same name), and the AWS profile is named every
# time, never the default.
CUSTOMER   := $(filter-out dev,$(ENV))
ACCOUNT_ID := $(if $(CUSTOMER),$(shell sed -n 's/^account_id *= *"\([0-9]*\)".*/\1/p' terraform/envs/$(ENV)/terraform.tfvars 2>/dev/null))
BOOTSTRAP_WORKSPACE := $(or $(CUSTOMER),default)
ifneq ($(CUSTOMER),)
ifeq ($(AWS_PROFILE),default)
$(error ENV=$(ENV) is a customer's account: name the AWS profile for it, e.g. AWS_PROFILE=$(ENV))
endif
endif

# Instance to target with viewer/shell/logs/soak: the test desktop, or
# DESKTOP=<name>'s (terraform output desktop_instance_ids).
INSTANCE ?= $(if $(DESKTOP),$(shell $(TF) output -json desktop_instance_ids 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin).get("$(DESKTOP)",""))'),$(shell $(TF) output -raw test_desktop_instance_id 2>/dev/null))
# Playbooks run on every running desktop; LIMIT=<instance id> (or DESKTOP=<name>) for one.
# An unknown DESKTOP matches no host rather than all of them.
LIMIT ?= $(if $(DESKTOP),$(or $(INSTANCE),no-desktop-named-$(DESKTOP)),)
PLAYBOOK_LIMIT = $(if $(LIMIT),--limit $(LIMIT),)
SSM_BUCKET = $(shell $(TF) output -raw ssm_transfer_bucket)

.DEFAULT_GOAL := help
.PHONY: help bootstrap init new-customer web-test web-dev web-smoke front-door-test front-door-build front-door-deploy plan apply destroy fmt lint galaxy desktop screenshot llm-keys agentd-test agentd-build agentd-deploy agentd-evals agentd-soak agentd-reset agentd-logs tailnet-build tailnet-deploy tailscale-key tailscale-oauth apns-key gatewayd-test gatewayd-build gateway-image app-tailscalekit app-test app-sim app-device app-mac app-release-mac app-testflight new-desktop tester-link tailnet-rules cdp viewer shell

help: ## Show targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-14s %s\n", $$1, $$2}'

# --- Terraform ---------------------------------------------------------------

bootstrap: ## Create the S3 state bucket (one time per account; local state)
	$(TF_BOOTSTRAP) init
	$(TF_BOOTSTRAP) workspace select -or-create $(BOOTSTRAP_WORKSPACE)
	$(TF_BOOTSTRAP) apply $(if $(CUSTOMER),-var account_id=$(or $(ACCOUNT_ID),$(error no account_id in terraform/envs/$(ENV)/terraform.tfvars)))

init: ## terraform init the env against the state bucket
	$(TF) init -backend-config="bucket=$$(TF_WORKSPACE=$(BOOTSTRAP_WORKSPACE) $(TF_BOOTSTRAP) output -raw state_bucket)" -backend-config="region=$(AWS_REGION)"

new-customer: ## A customer deployment's Terraform env (no AWS calls): NAME=<name> ACCOUNT=<their sandbox account id>
	scripts/new-customer-env.sh $(NAME) $(ACCOUNT)

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
	cd ansible && ansible-playbook --syntax-check playbooks/desktops.yml playbooks/screenshot.yml playbooks/tailnet-proxy.yml playbooks/agentd.yml playbooks/front-door.yml

# --- Ansible -----------------------------------------------------------------

# On macOS, forked Ansible workers segfault when botocore looks up the system
# proxy settings (_scproxy). no_proxy='*' skips that lookup.
ANSIBLE_PLAYBOOK := cd ansible && no_proxy='*' OBJC_DISABLE_INITIALIZE_FORK_SAFETY=YES ansible-playbook

galaxy: ## Install the Ansible collections
	cd ansible && ansible-galaxy collection install -r requirements.yml

new-desktop: ## A desktop for someone else on the desktops tailnet, step by step, with setup links: NAME=<name> (docs/onboarding.md)
	scripts/new-desktop.sh $(NAME)

tester-link: ## New setup links for a tester's devices (each works once, 3 days): NAME=<name> [DEVICES=2]
	scripts/tailnet.py link $(NAME) --devices $(or $(DEVICES),2)

tailnet-rules: ## Write the desktops tailnet's access rules from Terraform's desktops (shows them, asks first)
	scripts/tailnet.py policy

desktop: ## Run the desktop playbook against running desktops (LIMIT= or DESKTOP= for one; TAGS=chrome_policy for just Chrome's policy)
	$(ANSIBLE_PLAYBOOK) playbooks/desktops.yml $(PLAYBOOK_LIMIT) $(if $(TAGS),--tags $(TAGS)) -e ansible_aws_ssm_bucket_name=$(SSM_BUCKET)

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

agentd-evals: ## Memory evals on this Mac (cents per scenario): MEMORY_V2=on|off SCENARIO=name MODEL=id PROVIDER=together|bedrock
	cd services/agentd && cargo build --quiet
	$(if $(filter bedrock,$(PROVIDER)),,TOGETHER_API_KEY="$${TOGETHER_API_KEY:-$$(aws ssm get-parameter --name /cloud-desktops/$(ENV)/llm/together_api_key \
	  --with-decryption --query Parameter.Value --output text)}") \
	  services/agentd/evals/run.py --agentd services/agentd/target/debug/agentd \
	  --memory-v2 $(or $(MEMORY_V2),off) $(if $(SCENARIO),--scenario $(SCENARIO)) $(if $(MODEL),--model $(MODEL)) \
	  $(if $(PROVIDER),--provider $(PROVIDER)) $(if $(BEDROCK_REGION),--region $(BEDROCK_REGION))

agentd-soak: ## Memory v2 soak report for the desktop (read-only): SINCE=YYYY-MM-DD, the day the soak started
	scripts/agentd-soak.py $(INSTANCE) $(if $(SINCE),--since $(SINCE))

agentd-reset: ## Wipe a desktop clean: agentd's chat, memories, schedules, keys, and Chrome's cookies and profile (asks first): DESKTOP=name FILES=1 KEEP_COPY=1 RESTORE=1 DRY_RUN=1
	scripts/agentd-reset.py $(or $(INSTANCE),no-desktop-named-$(DESKTOP)) $(if $(FILES),--files) $(if $(KEEP_COPY),--keep-copy) \
	  $(if $(RESTORE),--restore) $(if $(DRY_RUN),--dry-run)

agentd-logs: ## Show agentd's recent log
	aws ssm start-session --target $(INSTANCE) --document-name AWS-StartInteractiveCommand \
	  --parameters '{"command":["sudo journalctl -u agentd -n 50 --no-pager"]}'

tailnet-build: ## Build tailnet-proxy (tsnet) for the desktop (linux, DESKTOP_ARCH)
	cd services/tailnet-proxy && CGO_ENABLED=0 GOOS=linux GOARCH=$(DESKTOP_ARCH) \
	  go build -trimpath -ldflags "-s -w" -o build/tailnet-proxy-linux-$(DESKTOP_ARCH) .

web-test: ## Unit tests for the web app's chat rules, Markdown and key detector (node)
	node --test apps/web/test/*.test.mjs

web-dev: ## The web app on this Mac, end to end: agentd with a scripted model, front-door, a stand-in for Cloudflare Access (http://127.0.0.1:8088/)
	apps/web/dev/run.sh

web-smoke: ## Click through the web app in headless Chrome (with make web-dev running)
	uv run -q --with websockets apps/web/dev/smoke.py $(or $(OUT),/tmp)

front-door-test: ## Vet and test front-door (a customer deployment's way in, behind Cloudflare Access)
	cd services/front-door && go vet ./... && go test ./...

front-door-build: ## Build front-door for the desktop (linux, DESKTOP_ARCH)
	cd services/front-door && CGO_ENABLED=0 GOOS=linux GOARCH=$(DESKTOP_ARCH) \
	  go build -trimpath -ldflags "-s -w" -o build/front-door-linux-$(DESKTOP_ARCH) .

front-door-deploy: front-door-build ## Put a customer deployment's desktops behind Cloudflare: front-door and cloudflared; deploy agentd first (ENV=<customer>)
	$(ANSIBLE_PLAYBOOK) playbooks/front-door.yml $(PLAYBOOK_LIMIT) -e ansible_aws_ssm_bucket_name=$(SSM_BUCKET)

gatewayd-test: ## Format check, clippy and unit tests for gatewayd (the attested model gateway)
	cd services/gatewayd && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test

gatewayd-build: ## Build gatewayd for the gateway image (static linux/arm64 binary)
	cd services/gatewayd && cargo zigbuild --release --target aarch64-unknown-linux-musl

gateway-image: gatewayd-build ## Build the attested gateway image (AMI) on a temporary builder; prints the AMI and its measurements
	ENV=$(ENV) scripts/build-gateway-image.sh

apns-key: ## Store the APNs push key in SSM: make apns-key FILE=AuthKey_XXXXXXXXXX.p8 KEY_ID=XXXXXXXXXX
	SSM_PREFIX=/cloud-desktops/$(ENV) scripts/put-apns-key.py "$(FILE)" "$(KEY_ID)"

tailscale-key: ## Store your personal tailnet's auth key (tagged tag:cloud-desktop) for the test desktop in SSM (prompted, not echoed)
	scripts/put-secret.py /cloud-desktops/$(ENV)/tailscale/auth_key

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

# notarytool's keychain profile: credentials are per Apple ID and team, not per
# app, so the one made for Selecta (same team, 4UVAPZYV4Q) works here.
NOTARY_PROFILE ?= selecta-notary

app-release-mac: ## The Mac app for others: Developer ID, notarized, in a DMG (docs/onboarding.md; NOTARY_PROFILE=)
	NOTARY_PROFILE=$(NOTARY_PROFILE) apps/apple/scripts/release-mac.sh

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
