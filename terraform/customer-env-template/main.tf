# The __ENV__ customer deployment (docs/customer-cloud.md), in their sandbox
# account. Made by scripts/new-customer-env.sh from
# terraform/customer-env-template; the stack itself is terraform/stacks/customer.

module "customer" {
  source = "../../stacks/customer"

  env                    = "__ENV__"
  permissions_boundary   = "arn:aws:iam::${var.account_id}:policy/cloud-desktops-boundary"
  desktops               = var.desktops
  desktop_instance_type  = var.desktop_instance_type
  enable_image_builder   = var.enable_image_builder
  gateway_ami            = var.gateway_ami
  model_daily_budget_usd = var.model_daily_budget_usd
}
