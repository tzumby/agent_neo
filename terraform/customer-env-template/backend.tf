terraform {
  # The state lives in the customer's account: bucket and region are passed
  # at init time from the bootstrap stack's output for this account (`make
  # bootstrap init ENV=__ENV__`).
  backend "s3" {
    key          = "envs/__ENV__/terraform.tfstate"
    encrypt      = true
    use_lockfile = true
  }
}
