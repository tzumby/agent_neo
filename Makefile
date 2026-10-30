AWS_PROFILE ?= default
AWS_REGION  ?= ca-central-1
ENV         ?= dev
export AWS_PROFILE AWS_REGION

TF_BOOTSTRAP := terraform -chdir=$(CURDIR)/terraform/bootstrap
TF           := terraform -chdir=$(CURDIR)/terraform/envs/$(ENV)

# Instance to target with viewer/shell/vnc-password (defaults to the test desktop).
INSTANCE ?= $(shell $(TF) output -raw test_desktop_instance_id 2>/dev/null)
SSM_BUCKET = $(shell $(TF) output -raw ssm_transfer_bucket)

.DEFAULT_GOAL := help
.PHONY: help bootstrap init plan apply destroy fmt lint galaxy desktop screenshot viewer vnc-password shell

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
	cd ansible && ansible-playbook --syntax-check playbooks/desktops.yml playbooks/screenshot.yml

# --- Ansible -----------------------------------------------------------------

# On macOS, forked Ansible workers segfault when botocore looks up the system
# proxy settings (_scproxy). no_proxy='*' skips that lookup.
ANSIBLE_PLAYBOOK := cd ansible && no_proxy='*' OBJC_DISABLE_INITIALIZE_FORK_SAFETY=YES ansible-playbook

galaxy: ## Install the Ansible collections
	cd ansible && ansible-galaxy collection install -r requirements.yml

desktop: ## Run the desktop playbook against running desktops
	$(ANSIBLE_PLAYBOOK) playbooks/desktops.yml -e ansible_aws_ssm_bucket_name=$(SSM_BUCKET)

screenshot: ## Download a screenshot of each desktop to screenshots/
	$(ANSIBLE_PLAYBOOK) playbooks/screenshot.yml -e ansible_aws_ssm_bucket_name=$(SSM_BUCKET)

# --- Access (SSM only) -------------------------------------------------------

viewer: ## Forward noVNC to http://localhost:6080/vnc.html
	aws ssm start-session --target $(INSTANCE) --document-name AWS-StartPortForwardingSession \
	  --parameters '{"portNumber":["6080"],"localPortNumber":["6080"]}'

vnc-password: ## Print the desktop's VNC password
	aws ssm start-session --target $(INSTANCE) --document-name AWS-StartInteractiveCommand \
	  --parameters '{"command":["sudo cat /etc/cloud-desktops/vnc-password; echo"]}'

shell: ## Open a shell on the desktop over SSM
	aws ssm start-session --target $(INSTANCE)
