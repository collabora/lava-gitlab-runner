# Lava gitlab runner

A gitlab runner implementation intended to bridge gitlab to
[Lava](https://lavasoftware.org/).


# Installation

The lava gitlab runner only requires network (https) access to both the gitlab
server and the lava server(s), so can pretty much run anywhere.

The runner requires a runner authentication token to connect to the gitlab
server and pick up jobs. The lava url and token are provided by the gitlab-ci
jobs so don't have to be configured on the runner.

## Creating a new runner

A new runner must be created on the GitLab server. This can be done using the
[runner creation API](https://docs.gitlab.com/ee/api/users.html#create-a-runner-linked-to-a-user),
or manually in the GitLab
[runner management web interface](https://docs.gitlab.com/ee/ci/runners/runners_scope.html).
Make sure to follow the runner creation with an authentication token workflow,
as the registration token workflow is deprecated.

One key parameter provided when creating the runner is `run_untagged=false` (or
leaving the `Run untagged jobs` box unchecked in the web interface), which will
make the runner *only* pickup jobs which matches its tags. This is important to
prevent the runner from picking up "normal" jobs which it will not be able to
process.

When the runner is created GitLab provides an authentication token starting
with `glrt-`. This token should be provided to the runner for its GitLab
connection, along with a system ID that identifies the machine on which the
runner is executed.

The system ID should be a unique string. GitLab doesn't currently require any
particular formatting, but it is recommended to follow the way the official
`gitlab-runner` creates system IDs:

- Deriving it from the machine ID, found in `/var/lib/dbus/machine-id` or
  `/etc/machine-id`, but concatenating the machine ID with the string
  "gitlab-runner", taking the first 12 characters of its SHA256 hash in hex
  form, and prepending it with `s_`.

- Generating a random 12-character string using letters and digits
  (`[a-z][A-Z][0-9]`), and prepending it with `r_`.

In either case the system ID should be recorded in a persistent storage, along
with the authentication token, and be passed to the `Runner::new()` function.

The token can be verified using a curl command like:

```shell
curl --request POST "https://GITLAB_URL/api/v4/runners/verify"  \
  --form "token=AUTHENTICATION_TOKEN" \
  --form "system_id=SYSTEM_ID"
```

This step is optional. If performed, it will pre-register the system ID with
the GitLab server. Otherwise the system ID will be registered the first time
the runner pings for jobs.

## Running the runner from the source tree

The runner can be build using Cargo like any Rust program. To run it the url to
the gitlab server, the token and the system ID have to be either provided on
the command line or in the `GITLAB_URL`, `GITLAB_TOKEN` and `RUNNER_SYSTEM_ID`
environment variables. For example to run directly from source `cargo run
https://gitlab.myserver.org RUNNER_TOKEN SYSTEM_ID`.

## Running from a docker image

A pre-built docker image can be found at
registry.gitlab.collabora.com/lava/lava-gitlab-runner/main. To connect to the
gitlab server, `GITLAB_URL` and `GITLAB_TOKEN` should be set in the docker
environment when running the container.

## Running in a Kubernetes cluster

A helm chart is provided in the repository for the runner (in the `chart`
directory). The main values in this chart that should be set are `gitlab.url` to
set the gitlab url and `gitlab.token` to set the gitlab token.

# Runner Usage

The runner is used with normal gitlab runner jobs in the same way as other
runners. The main difference is that rather then the script commands being run
inside a shell they're directly executed by the runner itself.

## Runner variables usage

* `LAVA_URL`: This lava server the job should work with
* `LAVA_TOKEN`: The lava authentication token

These variables will typically be set as (masked) CI variables, but they could
also be provided directly in the job.

## Runner supported commands

The following commands are currently supported in the job scripts:

`submit <filename>` - This submits the given filename from the dependencies to
the lava server and will follow the lava job's log output. The gitlab job will
fail if the lava job ends in an incomplete state or any other issue occurs
(e.g. submission failure)




`monitor-file <filename>` - This monitors the lava job ids in the given filename
from the dependencies. The job file should be a json file containing a
dictionary with a "jobids" field which holds an array of ids, e.g.
`{"jobids": [ 1, 2, 3, 4]}`. As multiple lava jobs can be monitored, the job
outputs will not be directly logged to the gitlab job log output, only the job
states are logged. The job will fail if one of the lava jobs finishes with an
incomplete state.

## Runner artifacts

When the gitlab job definition defines artifacts, the log file(s) of the
submitted or monitor jobs will be uploaded as an artifact by the runner.

## Example jobs

To submit a job to lava, monitor it and retrieve the raw log as a job artifact
the following snippet can be used. The referred `lava-job.yaml` should be a file
available in one of the artifacts from the job's dependencies.

```
lava submission:
  stage: lava
  timeout: 1d
  tags:
    - lava-runner
  script:
    - submit lava-job.yaml
  artifacts:
    when: always
    paths:
      - "*.yaml"
```

To monitor a set of jobs and retrieve their log files the following snippet can
be used.The referred `tests.json` should be a file available in one of the
artifacts from the job's dependencies.

```
lava submission:
  stage: lava
  timeout: 1d
  tags:
    - lava-runner
  script:
    - monitor-files test.json
  artifacts:
    when: always
    paths:
      - "*.yaml"
```
