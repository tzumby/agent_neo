# Credentials come from AWS_PROFILE (set by the Makefile; for a customer, our
# sign-in to their account), never a default. allowed_account_ids stops a
# plan against any other account, ours included.
provider "aws" {
  region              = var.region
  allowed_account_ids = [var.account_id]

  default_tags {
    tags = {
      Project   = "cloud-desktops"
      Env       = "__ENV__"
      ManagedBy = "terraform"
    }
  }
}
