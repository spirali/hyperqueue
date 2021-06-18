
# Jobs

A *job* is a portion of work that can be submited into a HyperQueue.

Each job may have one or *tasks*. In the current version, a is a single execution of a program.

**In this section, we are introducing simple jobs, where each job has exactly one tasks.**
See section about Job arrays for submiting more tasks in one job.

# Identification numbers

Each job is identified by a positive integer that is assigned by HyperQueue server when the job is submitted, we refer to it as *job id*.

Each task is identified by an unsigned 32b integer called *task id*. Task id can be assigned by a user and same task id may be used in two different jobs.

In simple jobs, task id is set to 0.


## Submiting jobs

``hq submit <program> <args1> ...``

HyperQueue assigns a unique job id when a job is submitted.

When your command contains its own switches, you need to use ``--`` after submit:

``hq submit -- /bin/bash -c 'echo $PPID'``


## Name of a job

Each job has assigned a name. It has only an informative character for the user. By default, the name is extracted from the job's program name. You can set a job name explicitly by:

``hq submit --name=<NAME> ...``


## Information about jobs

List of all jobs:

``hq jobs``


List of jobs in a defined state:

``hq jobs <filters...>``

Where filters can be: ``waiting``, ``running``, ``finished``, ``failed``, or ``canceled``.

Detailed information about a job:

``hq job <job-id>``


## Output of the job

By default, job produces two files named ``stdout.<job-id>.<task-id>`` and ``stderr.<job-id>.<task-id>``
that contains standard output and standard error output in the.
The files are by default placed in the directory where the job was submitted.

This can be changed via options ``--stdout=<path>`` and ``--stderr=<path>``.
These set paths where stdout/stderr files are created. Placeholders
``%{JOB_ID}`` and ``%{TASK_ID}`` in a path will be replaced to the JOB_ID/TASK_ID.

You can disable creating stdout/stderr completely by setting value ``none``.
(``--stdout=none`` / ``--stderr=none``).


!!! Hint

    If you run many jobs/tasks, they might produce an unwieldy amount of stdout/stderr files. [Here](cookbook/handling_many_files.md) you can read how you handle this situation.

## Task states

```
Submitted
   |
   |
   v
Waiting-----------------\
   | ^                  |
   | |                  |
   v |                  |
Running-----------------|
   | |                  |
   | \--------\         |
   |          |         |
   v          v         v
Finished    Failed   Canceled
```

* *Submitted* - Only an informative state that a submission was successfull; it is immediately changed into "Waiting" state.
* *Waiting* - The task is waiting for start
* *Running* - The task is running in a worker. It may become "waiting" again when a worker (where the task is running) is lost.
* *Finished* - The task was sucessfully finished.
* *Failed* - The task failed. The error can be shown by ``hq job <job-id>``.
* *Canceled* -  The task was canceled by a user.


## Job states

In simple jobs, job state correspondes directly to the state of its a single task. In case of task arrays, see the chapter about task arrays.


## Canceling jobs

``hq cancel <job-id>``

A job cannot be canceled if it is already finished, failed, or canceled.



## Priorities

Not released yet, **scheduled for release v0.3**

