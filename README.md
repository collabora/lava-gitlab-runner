# Lava gitlab runner

A gitlab runner implementation intended to bridge gitlab to
[Lava](https://lavasoftware.org/).


# Installation

The lava gitlab runner only requires network (https) access to both the gitlab
server and the lava server(s), so can pretty much run anywhere.

The runner requires a runner token to connect to the gitlab server and pick up
jobs. The lava url and token are provided by the gitlab-ci jobs so don't have
to be configured on the runner.

## Registering a new runner

The runner cannot register itself with the gitlab server, so this has to be
done by hand using the gitlab
[runner registration API](https://docs.gitlab.com/ee/api/runners.html#register-a-new-runner).

The registration token can be retrieved from the runners section in the Gitlab
administration area. With that token the runner can be register using a curl
command like:
```
curl --request POST "https://GITLAB_URL/api/v4/runners"  \
  --form "description=Lava runner" \
  --form "run_untagged=false" \
  --form "tag_list=lava-runner" \
  --form "token=REGISTRATION_TOKEN"
```

As a response to this command a new token for the registered runner will be
provided, this token should be provided to the runner for it's gitlab
connection.

One thing to key parameter provided here is `run_untagged=false`, which will
make the runner *only* pickup jobs which matches its tag. This is important to
prevent the runner from picking up "normal" jobs which it will not be able to
process.

## Running the runner from the source tree

The runner can be build using Cargo like any Rust program. To run it the url to
the gitlab server and the token have to be either provided on the command line
or in the `GITLAB_URL` and `GITLAB_TOKEN` environment variables. For example to
run directly from source `cargo run https://gitlab.myserver.org RUNNER_TOKEN`.

## Running from a docker image

A pre-build docker image can be found at
registry.gitlab.collabora.com/lava/lava-gitlab-runner/main. To connect to the
gitlab server `GITLAB_URL` and `GITLAB_TOKEN` should be set in the docker
environment when running the container.

## Running in a Kubernetes cluster

A helm chart is provided in the repository for the runner (in the chart
directory). The main values in this chart that should be set are `gitlab.url` to
set the gitlab url and `gitlab.token` to set the gitlab token.

# Runner Usage

The runner is used with normal gitlab runner jobs, same as any other runner.
The main difference is that rather then the script commands being run inside a
shell they're directly and executed by the runner itself.

## Runner variables usage

* `LAVA_URL`: This defines the lava server this job should work with
* `LAVA_TOKEN`: The lava authentication token

These variables will typically be set as (masked) CI variables, but they could
also be provided directly in the job.

## Runner supported commands

The following commands are currently supported in the job scripts:

`submit <filename>` - This submit the given filename from the dependencies to
the to the lava server and will follow the lava jobs log output. The gitlab job will
fail if the lava job ends in an incomplete state or any other issue occurs
(e.g. submission failure)




`monitor-file <filename>` - This monitors the lava job ids in the given filename
from the dependencies. The job file should be a json file containing a
dictionary with a "jobids" field which holds an array of ids. e.g.
`{"jobids": [ 1, 2, 3, 4]}`. As multiple lava jobs can be monitored the job
outputs will not be directly logged to the gitlab job log output, only the job
states are logged. The job will fail if one of the lava jobs finishes with an
incomplete state.

## Runner artifacts

When the gitlab job definition defines artifacts the log file(s) of the
submitted or monitor jobs will be uploaded as an artifact by the runner.

## Example jobs

To submit a job to lava , monitor it and retrieve the raw log as a job artifact
the following snippet can be used. The referred `lava-job.yaml` should be a file
available in one of the artifacts from the jobs dependencies.

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
      - *.yaml
```

To monitor a set of jobs and retrieve their log files the following snippet can
be used.The referred `tests.json` should be a file available in one of the
artifacts from the jobs dependencies.

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
      - *.yaml
```
