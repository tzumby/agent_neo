AWS_PROFILE ?= default
AWS_REGION  ?= ca-central-1
# This deployment's name: in resource names, tags, SSM paths
# (/neo-agent/<env>/...) and the state bucket. A second deployment in the
# same account needs its own (ENV=other).
ENV         ?= personal
# The desktop's architecture (Terraform: desktop_arch): amd64 or arm64.
DESKTOP_ARCH ?= amd64
DESKTOP_RUST_TARGET = $(if $(filter arm64,$(DESKTOP_ARCH)),aarch64,x86_64)-unknown-linux-musl
export AWS_PROFILE AWS_REGION
# Terraform reads these as var.region and var.env.
export TF_VAR_region := $(AWS_REGION)
export TF_VAR_env    := $(ENV)

TF_BOOTSTRAP := terraform -chdir=$(CURDIR)/terraform/bootstrap
TF           := terraform -chdir=$(CURDIR)/terraform/personal
SSM_PREFIX   := /neo-agent/$(ENV)

INSTANCE   = $(shell $(TF) output -raw desktop_instance_id 2>/dev/null)
SSM_BUCKET = $(shell $(TF) output -raw ssm_transfer_bucket)

.DEFAULT_GOAL := help
.PHONY: help preflight bootstrap init plan apply destroy fmt lint galaxy inventory deploy desktop screenshot \
  model-key tailscale-key tailscale-allow apns-key agentd-test helper-test agentd-build agentd-deploy agentd-evals agentd-reset agentd-logs agentd-telemetry \
  tailnet-test tailnet-build tailnet-deploy url web-test web-dev web-smoke \
  app-tailscalekit app-project app-test app-sim app-device app-mac stop start cdp viewer shell

help: ## Show targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-15s %s\n", $$1, $$2}'

# --- Terraform ---------------------------------------------------------------

# AWS_PROFILE_ORIGIN tells it whether AWS_PROFILE is yours or the default above.
preflight: ## Check this account and region can take a deployment: permissions, SCPs, quota (read-only)
	AWS_PROFILE_ORIGIN=$(origin AWS_PROFILE) scripts/preflight.py $(ENV)

bootstrap: ## Create the S3 state bucket (one time; its own state stays local, in terraform/bootstrap)
	$(TF_BOOTSTRAP) init
	$(TF_BOOTSTRAP) workspace select -or-create $(ENV)
	$(TF_BOOTSTRAP) apply

init: ## terraform init against the state bucket
	$(TF) init -reconfigure -backend-config="bucket=$$(TF_WORKSPACE=$(ENV) $(TF_BOOTSTRAP) output -raw state_bucket)" -backend-config="region=$(AWS_REGION)"

plan: ## terraform plan, saved to tfplan
	$(TF) plan -out=tfplan

apply: ## Apply the saved tfplan
	$(TF) apply tfplan

destroy: ## terraform destroy (the state bucket stays: prevent_destroy)
	$(TF) destroy

fmt: ## Format Terraform code
	terraform fmt -recursive terraform

lint: inventory ## Check formatting, validate Terraform, syntax-check playbooks
	terraform fmt -check -recursive terraform
	$(TF) validate
	cd ansible && ansible-playbook --syntax-check playbooks/desktops.yml playbooks/screenshot.yml playbooks/tailnet-proxy.yml playbooks/agentd.yml

# --- Keys (SSM, prompted, never echoed) --------------------------------------

model-key: ## Optional: store a model key in SSM (Anthropic's, or PROVIDER=together); else paste it in the app
	scripts/put-secret.py $(SSM_PREFIX)/llm/$(if $(filter together,$(PROVIDER)),together,anthropic)_api_key

tailscale-key: ## Store a Tailscale auth key (tagged tag:neo-agent) in SSM
	scripts/put-secret.py $(SSM_PREFIX)/tailscale/auth_key

tailscale-allow: ## Optional: only these Tailscale logins get in (comma-separated), then make tailnet-deploy
	scripts/put-secret.py $(SSM_PREFIX)/tailscale/allowed_logins

apns-key: ## Optional: store your Apple APNs key for the apps' notifications in SSM, then make agentd-deploy
	scripts/put-apns-key.py $(SSM_PREFIX)/apns

# --- Ansible -----------------------------------------------------------------

# On macOS, forked Ansible workers segfault when botocore looks up the system
# proxy settings (_scproxy). no_proxy='*' skips that lookup.
ANSIBLE_PLAYBOOK := cd ansible && no_proxy='*' OBJC_DISABLE_INITIALIZE_FORK_SAFETY=YES ansible-playbook
ANSIBLE_ARGS      = -e ansible_aws_ssm_bucket_name=$(SSM_BUCKET)

galaxy: ## Install the Ansible collections
	cd ansible && ansible-galaxy collection install -r requirements.yml

# The EC2 inventory plugin can't read the region or env from the environment.
inventory:
	@sed -e 's/@REGION@/$(AWS_REGION)/' -e 's/@ENV@/$(ENV)/' ansible/inventory/aws_ec2.yml.tmpl > ansible/inventory/aws_ec2.yml

deploy: desktop agentd-deploy tailnet-deploy ## Everything on the desktop, in order: desktop, agentd, tailnet-proxy

desktop: inventory ## The desktop itself: hardening, X, XFCE, Chrome, VNC (TAGS=chrome_policy for just Chrome's policy)
	$(ANSIBLE_PLAYBOOK) playbooks/desktops.yml $(if $(TAGS),--tags $(TAGS)) $(ANSIBLE_ARGS)

screenshot: inventory ## Download a screenshot of the desktop to screenshots/
	$(ANSIBLE_PLAYBOOK) playbooks/screenshot.yml $(ANSIBLE_ARGS)

# --- agentd ------------------------------------------------------------------

agentd-test: ## Format check, clippy and unit tests for agentd
	cd services/agentd && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test

helper-test: ## Unit tests for the desktop's privileged worker and proxy helper (python, uv)
	uv run -q --with jinja2 --with pyyaml python -I -m unittest discover -s ansible/tests

agentd-build: ## Build agentd for the desktop (static linux binary for DESKTOP_ARCH)
	cd services/agentd && cargo zigbuild --release --target $(DESKTOP_RUST_TARGET)

agentd-deploy: agentd-build inventory ## Install agentd on the desktop
	$(ANSIBLE_PLAYBOOK) playbooks/agentd.yml $(ANSIBLE_ARGS)

agentd-logs: ## Show agentd's recent log
	aws ssm start-session --target $(INSTANCE) --document-name AWS-StartInteractiveCommand \
	  --parameters '{"command":["sudo journalctl -u agentd -n 50 --no-pager"]}'

agentd-reset: ## Wipe the agent clean: chat, memories, schedules, keys, and Chrome's profile (asks first): FILES=1 KEEP_COPY=1 RESTORE=1 DRY_RUN=1
	scripts/agentd-reset.py $(INSTANCE) $(if $(FILES),--files) $(if $(KEEP_COPY),--keep-copy) \
	  $(if $(RESTORE),--restore) $(if $(DRY_RUN),--dry-run)

agentd-telemetry: ## Where the model tokens go, per task (DAYS=7); one task call by call (TASK=id); all rows to a file (EXPORT=file.json)
	scripts/agentd-telemetry.py $(INSTANCE) --days $(or $(DAYS),7) $(if $(TASK),--task $(TASK)) $(if $(EXPORT),--export $(EXPORT))

agentd-evals: ## Memory evals on this machine against Together (cents per scenario): MEMORY_V2=on|off SCENARIO=name MODEL=id PROVIDER=together|bedrock
	cd services/agentd && cargo build --quiet
	$(if $(filter bedrock,$(PROVIDER)),,TOGETHER_API_KEY="$${TOGETHER_API_KEY:-$$(aws ssm get-parameter --name $(SSM_PREFIX)/llm/together_api_key \
	  --with-decryption --query Parameter.Value --output text)}") \
	  services/agentd/evals/run.py --agentd services/agentd/target/debug/agentd \
	  --memory-v2 $(or $(MEMORY_V2),off) $(if $(SCENARIO),--scenario $(SCENARIO)) $(if $(MODEL),--model $(MODEL)) \
	  $(if $(PROVIDER),--provider $(PROVIDER)) $(if $(BEDROCK_REGION),--region $(BEDROCK_REGION))

# --- tailnet-proxy and the web app -------------------------------------------

tailnet-test: ## Vet and test tailnet-proxy (and package door)
	cd services/tailnet-proxy && go vet ./... && go test ./...

tailnet-build: ## Build tailnet-proxy (tsnet) for the desktop (linux, DESKTOP_ARCH)
	cd services/tailnet-proxy && CGO_ENABLED=0 GOOS=linux GOARCH=$(DESKTOP_ARCH) \
	  go build -trimpath -ldflags "-s -w" -o build/tailnet-proxy-linux-$(DESKTOP_ARCH) .

tailnet-deploy: tailnet-build inventory ## Put the desktop on your tailnet: the web app, agent API, noVNC, VNC (deploy agentd first; needs make tailscale-key)
	$(ANSIBLE_PLAYBOOK) playbooks/tailnet-proxy.yml $(ANSIBLE_ARGS)

url: inventory ## Show the web app's address on your tailnet (https://<name>.<tailnet>.ts.net)
	$(ANSIBLE_PLAYBOOK) playbooks/tailnet-proxy.yml --tags url $(ANSIBLE_ARGS)

web-test: ## Unit tests for the web app's chat rules, Markdown and key detector (node)
	node --test apps/web/test/*.test.mjs

web-dev: ## The web app on this machine, end to end: agentd with a scripted model, served by devdoor (http://127.0.0.1:8088/)
	apps/web/dev/run.sh

web-smoke: ## Click through the web app in headless Chrome (with make web-dev running)
	uv run -q --with websockets apps/web/dev/smoke.py $(or $(OUT),/tmp)

# --- The iPhone and Mac apps (apps/apple) ------------------------------------

# Your Apple developer team (10 letters and digits); apps/apple/scripts/project.sh
# lists the ones this Mac can sign for. APP_BUNDLE_ID is optional. APP_PUSH=1
# turns on notifications (needs the paid Apple Developer Program; off by default
# so the apps build on a free account).
export APPLE_TEAM_ID APP_BUNDLE_ID APP_PUSH

app-tailscalekit: ## Once: build TailscaleKit, the apps' own Tailscale node (Go + Xcode, a few minutes)
	apps/apple/scripts/build-tailscalekit.sh

app-project: ## Generate apps/apple/NeoAgent.xcodeproj for Xcode, signed by your team (APPLE_TEAM_ID)
	apps/apple/scripts/project.sh

app-test: ## Unit tests for the apps' shared code (DesktopKit, on this Mac)
	cd apps/apple/DesktopKit && swift test

# A compile check: any team ID will do, nothing is signed.
app-sim: ## Build the iPhone app for the Simulator (no signing)
	APPLE_TEAM_ID=$(or $(APPLE_TEAM_ID),AAAAAAAAAA) apps/apple/scripts/project.sh
	cd apps/apple && xcodebuild -project NeoAgent.xcodeproj -scheme NeoAgent \
	  -destination 'generic/platform=iOS Simulator' -derivedDataPath build/sim CODE_SIGNING_ALLOWED=NO build -quiet

app-device: ## Build, install and open the iPhone app on your iPhone (cable, unlocked; APPLE_TEAM_ID)
	apps/apple/scripts/run-on-device.sh

app-mac: ## Build the Mac app, install it in /Applications and open it (APPLE_TEAM_ID)
	apps/apple/scripts/run-on-mac.sh

# --- Power -------------------------------------------------------------------

stop: ## Stop the desktop to save money (its disk stays; schedules don't run while it's off)
	aws ec2 stop-instances --instance-ids $(INSTANCE) --query 'StoppingInstances[0].CurrentState.Name' --output text

start: ## Start the desktop again (back on your tailnet in a minute or two)
	aws ec2 start-instances --instance-ids $(INSTANCE) --query 'StartingInstances[0].CurrentState.Name' --output text

# --- Access (SSM only) -------------------------------------------------------

cdp: ## Forward Chrome's debugging port to localhost:9222 (full control of the logged-in browser while open)
	aws ssm start-session --target $(INSTANCE) --document-name AWS-StartPortForwardingSession \
	  --parameters '{"portNumber":["9222"],"localPortNumber":["9222"]}'

viewer: ## Forward noVNC to http://localhost:6080/vnc.html
	aws ssm start-session --target $(INSTANCE) --document-name AWS-StartPortForwardingSession \
	  --parameters '{"portNumber":["6080"],"localPortNumber":["6080"]}'

shell: ## Open a shell on the desktop over SSM
	aws ssm start-session --target $(INSTANCE)
