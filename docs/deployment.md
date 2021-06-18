
# Deployment

This section describes, how to HyperQueue server


## Starting server

Server may run on any computer as long as computing nodes are able to connect to these machine. It is not necessary to be able to connect from
server to computing nodes. In the most simple scenario, we expect that the user starts its own instance of HyperQueue directly on login of a HPC system.

The server can be simply started by the following command:

``hq server start``


!!! Note

    The server opens two TCP/IP ports: one for submitting jobs and one for connecting workers. By default, these ports
    are automatically assigned by the operation system. You do not have to remember them, as they are stored in the
    "server directory". Other components automatically read these settings.


## Server directory

When a HQ server is started, it creates a server directory where it stores informations needed for submiting jobs and connecting workers.

**Important:** Encryption keys are stored in the server directory. Who has access to server directory may submit jobs, connect workers to HyperQueue instance, and decrypt communication between HyperQueue components.

By default, server directory is stored in ``$HOME/.hq-server``. It may be changed via option ``--server-dir=<PATH>``. In such case,
all commands need to use the ``--server-dir`` settings.

You can run more instances of HyperQueue under the same user. All you need is to set a different server directories for each instance.


## Stopping server

A server can be stopped by command:

``hq server stop``


## Starting worker

A worker can be started by command. It reads server directory and connectes to the server.

``hq worker start``

### Starting worker in PBS

``qsub <qsub-settings> -- hq worker start``


### Starting worker in SLURM

``sbatch <qsub-settings> -- hq worker start``


## List of workers

``hq worker list``


## Stopping worker

``hq worker stop <id>``


## CPUs configuration

Worker automatically detect number of CPUs and on Linux system it also detects partitioning into sockets.
In most cases, it should work without need of any touch. If you want to see how is your seen by
a worker without actually starting it, you can start ``$ hq worker hwdetect`` that only prints CPUs layout.

### Manual specification of CPU configration

If automatic detection fails, or you want to manually configure set CPU configuration, you can use
``--cpus`` parameter; for example as follows:

- 8 CPUs for worker
  ``$ hq worker start --cpus=8``

- 2 sockets with 12 cores per socket
  ``$ hq worker --cpus=2x12``