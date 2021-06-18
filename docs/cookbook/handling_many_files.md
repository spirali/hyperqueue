# Handling many output files
Each task/job produces a stdout/stderr file, and if you run many of them, you might run into problems
caused by the creation of so many files. This is exacerbated especially on HPC systems that use a shared distributed
file system. On this page you can find several ways of tackling this issue.

## Disabling stdout/stderr logging
The simplest option is to pass `--stdout=none`/`--stderr=none` when submitting a job to HQ. With these flags, stdout
and stderr will not be stored on disk. Even though this solution is simple, it's often not very useful, as you might
want to read the contents of these files after the job finishes.

## Storing output files on a temporary or scratch disk
If you are having performance problems caused by too many created output files, you can store these files on a different,
faster filesystem. HPC systems often offer these fast filesystems out of the box, mounted e.g. on `/tmp` or `/ramdisk`
paths.

```bash
$ hq submit \
  --stdout=/tmp/stdout.%{JOB_ID}.%{TASK_ID}.txt \
  --stderr=/tmp/stderr.%{JOB_ID}.%{TASK_ID}.txt \
  <my-program-args>
```

This can alleviate the performance problem, but it might introduce another problem. Fast filesystems, such as RAM-disks,
are usually local to a node and not shared with other nodes. If you want to access files on these filesystems, you might
have to manually connect to the corresponding node to download the files. What's worse, these filesystems are often not
persistent, so their contents might disappear after the corresponding worker stops.

Often you might not actually care about all of these files, but you want to use them only for debugging your jobs, if
some of them fail. The best case scenario for this use-case could look like this:

- Write the output files to a fast filesystem to avoid performance bottlenecks
- Delete the files if the job succeeds
- Store the files to a persistent disk if the job fails to enable easy debugging

Luckily, HQ contains a utility command that enables you to perform this use-case easily.

### HQ backup script
HQ contains a utility command called `backup-output`. If you wrap your submitted program with this utility, it will
perform the operation described above.

```bash
$ hq submit \
  --stdout=/tmp/stdout.%{JOB_ID}.%{TASK_ID}.txt \
  --stderr=/tmp/stderr.%{JOB_ID}.%{TASK_ID}.txt \
  -- hq script backup-output /home/foo/backup \
  -- <my-program-args>
```

With the above script, the following will happen:

- If your program succeeds (with a zero exit code), its output files will be immediately deleted.
- If your program fails (with a non-zero exit code), its output files will be copied to `/home/foo/backup`. The wrapper
  will propagate the exit status of your program so that the job/task will be marked as failed.
