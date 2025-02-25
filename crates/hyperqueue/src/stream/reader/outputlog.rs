use crate::client::commands::outputlog::{CatOpts, Channel, ExportOpts, ShowOpts};
use crate::common::arraydef::IntArray;
use crate::common::error::HqError;
use crate::common::serialization::SerializationConfig;
use crate::stream::StreamSerializationConfig;
use crate::transfer::stream::{ChannelId, StreamChunkHeader};
use crate::worker::streamer::{StreamFileHeader, STREAM_FILE_HEADER, STREAM_FILE_SUFFIX};
use crate::{JobId, JobTaskId, Set};
use bincode::Options;
use chrono::{DateTime, Utc};
use colored::{Color, Colorize};
use lru::LruCache;
use serde::{Deserialize, Serialize};
use serde_json::json;
use smallvec::SmallVec;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, Write};
use std::num::NonZeroUsize;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use tako::InstanceId;

#[derive(Clone)]
pub struct ChunkInfo {
    time: DateTime<Utc>,
    position: u64,
    size: u32, // Currently chunk is actually limited to 128kB
}

pub struct InstanceInfo {
    instance_id: InstanceId,
    channels: [Vec<ChunkInfo>; 2],
    file_idx: usize,
    finished: bool,
}

#[derive(Default)]
pub struct TaskInfo {
    instances: SmallVec<[InstanceInfo; 1]>,
}

impl InstanceInfo {
    fn channel_size(&self, channel_id: ChannelId) -> u64 {
        self.channels[channel_id as usize]
            .iter()
            .map(|x| x.size as u64)
            .sum()
    }
}

impl TaskInfo {
    pub fn last_instance(&self) -> &InstanceInfo {
        self.instances.last().unwrap()
    }

    pub fn superseded(&self) -> impl Iterator<Item = &InstanceInfo> {
        self.instances[0..self.instances.len() - 1].iter()
    }

    pub fn instance_mut(&mut self, instance_id: InstanceId) -> Option<&mut InstanceInfo> {
        self.instances
            .iter_mut()
            .find(|info| info.instance_id == instance_id)
    }

    pub fn instance(&self, instance_id: InstanceId) -> Option<&InstanceInfo> {
        self.instances
            .iter()
            .find(|info| info.instance_id == instance_id)
    }
}

type StreamIndex = BTreeMap<JobId, BTreeMap<JobTaskId, TaskInfo>>;

/// Reader of a directory with .hts (stream files)
/// It creates an index over all jobs and tasks in stream
pub struct OutputLog {
    paths: Vec<PathBuf>,
    index: StreamIndex,
    cache: LruCache<usize, BufReader<File>>,
}

#[derive(Serialize, Deserialize)]
pub struct Summary {
    pub n_files: u32,
    pub n_jobs: u32,
    pub n_tasks: u64,
    pub n_streams: u64,
    pub n_opened: u64,
    pub stdout_size: u64,
    pub stderr_size: u64,
    pub n_superseded: u64,
    pub superseded_stdout_size: u64,
    pub superseded_stderr_size: u64,
}

impl OutputLog {
    pub fn open(path: &Path, server_uid: Option<&str>) -> crate::Result<Self> {
        log::debug!("Reading output log {}", path.display());
        let mut paths = Vec::new();
        let mut server_uids: Set<String> = Set::new();
        let mut found = false;
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();
            if path
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s == STREAM_FILE_SUFFIX)
                .unwrap_or(false)
            {
                found = true;
                log::debug!("Discovered {}", path.display());
                let mut file = BufReader::new(File::open(&path)?);
                let header = match OutputLog::check_header(&mut file) {
                    Ok(header) => header,
                    Err(e) => {
                        log::debug!(
                            "Skipping file {}, because reading header fails: {}",
                            path.display(),
                            e
                        );
                        continue;
                    }
                };
                if let Some(uid) = server_uid {
                    if uid != header.server_uid.as_str() {
                        log::debug!("{} ignored because different server uid", path.display());
                        continue;
                    }
                }
                server_uids.insert(header.server_uid.into_owned());
                paths.push(path);
            }
        }

        if !found {
            return Err(HqError::GenericError("No log files found".to_string()));
        }

        if server_uids.len() > 1 {
            use std::fmt::Write;
            let mut msg = "Found streams from multiple server instances:\n".to_string();
            for server_uid in server_uids {
                writeln!(msg, "{server_uid}").unwrap();
            }
            msg.push_str("Use --server-uid=... to specify server uid");
            return Err(HqError::GenericError(msg));
        }
        let index = Self::create_index(&paths)?;
        Ok(OutputLog {
            paths,
            index,
            cache: LruCache::new(NonZeroUsize::new(16).unwrap()),
        })
    }

    fn read_chunk(file: &mut BufReader<File>) -> crate::Result<Option<StreamChunkHeader>> {
        match StreamSerializationConfig::config().deserialize_from(file) {
            Ok(event) => Ok(Some(event)),
            Err(error) => match error.deref() {
                bincode::ErrorKind::Io(e)
                    if matches!(e.kind(), std::io::ErrorKind::UnexpectedEof) =>
                {
                    Ok(None)
                }
                _ => Err(error.into()),
            },
        }
    }

    fn create_index(paths: &[PathBuf]) -> crate::Result<StreamIndex> {
        let mut index: StreamIndex = BTreeMap::new();
        for (file_idx, path) in paths.iter().enumerate() {
            let mut file = BufReader::new(File::open(path)?);
            let _header = OutputLog::check_header(&mut file)?;
            while let Some(chunk_header) = Self::read_chunk(&mut file)? {
                let job = index.entry(chunk_header.job).or_default();
                let task = job.entry(chunk_header.task).or_default();
                if task
                    .instances
                    .last()
                    .map(|x| x.instance_id != chunk_header.instance)
                    .unwrap_or(true)
                {
                    task.instances.push(InstanceInfo {
                        instance_id: chunk_header.instance,
                        channels: [Vec::new(), Vec::new()],
                        file_idx,
                        finished: false,
                    });
                };
                let instance = task.instances.last_mut().unwrap();
                if chunk_header.size > 0 {
                    instance.channels[chunk_header.channel as usize].push(ChunkInfo {
                        time: chunk_header.time,
                        size: chunk_header.size as u32,
                        position: file.stream_position().unwrap(),
                    });
                    file.seek_relative(chunk_header.size as i64)?;
                } else {
                    instance.finished = true;
                }
            }
        }
        for job in index.values_mut() {
            for task in job.values_mut() {
                task.instances.sort_by_key(|x| x.instance_id);
            }
        }
        Ok(index)
    }

    fn check_header(file: &mut BufReader<File>) -> anyhow::Result<StreamFileHeader> {
        let mut header = [0u8; STREAM_FILE_HEADER.len()];
        file.read_exact(&mut header)?;
        if header != STREAM_FILE_HEADER {
            anyhow::bail!("Invalid file format");
        }
        Ok(StreamSerializationConfig::config().deserialize_from(file)?)
    }

    pub fn summary(&self) -> Summary {
        let n_opened = self
            .index
            .values()
            .flat_map(|x| x.values())
            .map(|infos| u64::from(!infos.last_instance().finished))
            .sum::<u64>();

        let n_streams = self
            .index
            .values()
            .flat_map(|x| x.values())
            .map(|infos| infos.instances.len() as u64)
            .sum::<u64>();

        let mut stdout_size = 0u64;
        let mut stderr_size = 0u64;
        let mut superseded_stdout_size = 0u64;
        let mut superseded_stderr_size = 0u64;

        for task_info in self.index.values().flat_map(|x| x.values()) {
            let info = task_info.last_instance();
            stdout_size += info.channel_size(0);
            stderr_size += info.channel_size(1);

            for info in task_info.superseded() {
                superseded_stdout_size += info.channel_size(0);
                superseded_stderr_size += info.channel_size(1);
            }
        }

        let n_tasks = self.index.values().map(|x| x.len() as u64).sum();
        Summary {
            n_files: self.paths.len() as u32,
            n_jobs: self.index.len() as u32,
            n_tasks,
            n_streams,
            n_opened,
            n_superseded: n_streams - n_tasks,
            stdout_size,
            stderr_size,
            superseded_stderr_size,
            superseded_stdout_size,
        }
    }

    fn _gather_infos<'a>(
        index: &'a StreamIndex,
        job_id: JobId,
        tasks: &Option<IntArray>,
    ) -> anyhow::Result<Vec<(JobTaskId, &'a InstanceInfo)>> {
        let job = index
            .get(&job_id)
            .ok_or_else(|| anyhow::format_err!("Job {job_id} not found"))?;
        Ok(match tasks {
            Some(array) => {
                let mut infos = Vec::new();
                for task_id in array.iter() {
                    if let Some(task_info) = job.get(&JobTaskId::new(task_id)) {
                        infos.push((JobTaskId::new(task_id), task_info.last_instance()));
                    } else {
                        anyhow::bail!("Task {} not found", task_id);
                    }
                }
                infos
            }
            None => job
                .iter()
                .map(|(&task_id, task_info)| (task_id, task_info.last_instance()))
                .collect(),
        })
    }

    fn read_buffer(
        cache: &mut LruCache<usize, BufReader<File>>,
        paths: &[PathBuf],
        file_idx: usize,
        position: u64,
        buffer: &mut [u8],
    ) -> anyhow::Result<()> {
        let file = cache.try_get_or_insert_mut(file_idx, || -> anyhow::Result<_> {
            Ok(BufReader::new(File::open(&paths[file_idx])?))
        })?;
        // We are using seek_relative which makes things slightly
        // more complicated, but it does not drop caches if it is not necessary
        // in comparison to file.seek
        let current_pos = file.stream_position().unwrap();
        let diff = position as i64 - current_pos as i64;
        file.seek_relative(diff)?;
        file.read_exact(buffer)?;
        Ok(())
    }

    pub fn export(&mut self, opts: &ExportOpts) -> anyhow::Result<()> {
        let task_infos = Self::_gather_infos(&self.index, opts.job, &opts.task)?;
        let mut result = Vec::new();
        let mut buffer = Vec::new();
        for (task_id, instance) in &task_infos {
            buffer.resize(instance.channel_size(0) as usize, 0u8);
            let mut buf_pos = 0;
            for chunk in &instance.channels[0] {
                let size = chunk.size as usize;
                Self::read_buffer(
                    &mut self.cache,
                    &self.paths,
                    instance.file_idx,
                    chunk.position,
                    &mut buffer[buf_pos..buf_pos + size],
                )?;
                buf_pos += size;
            }
            debug_assert!(buf_pos == buffer.len());
            result.push(json!({
                "id": task_id,
                "finished": instance.finished,
                "stdout": String::from_utf8_lossy(&buffer),
            }));
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::Value::Array(result))
                .expect("Could not format JSON")
        );
        Ok(())
    }

    pub fn jobs(&mut self) -> anyhow::Result<()> {
        let stdout = std::io::stdout();
        for job_id in self.index.keys() {
            writeln!(&stdout, "{}", job_id)?;
        }
        Ok(())
    }

    pub fn cat(&mut self, opts: &CatOpts) -> anyhow::Result<()> {
        let selected_channel_id = match opts.channel {
            Channel::Stdout => 0,
            Channel::Stderr => 1,
        };
        let mut buffer = Vec::new();
        let stdout = std::io::stdout();
        let mut stdout_buf = BufWriter::new(stdout.lock());

        let task_infos = Self::_gather_infos(&self.index, opts.job, &opts.task)?;

        if !opts.allow_unfinished {
            for (task_id, instance) in &task_infos {
                if !instance.finished {
                    anyhow::bail!("Stream for task {} is not finished", task_id);
                }
            }
        }

        for (_, instance) in &task_infos {
            for chunk in &instance.channels[selected_channel_id] {
                buffer.resize(chunk.size as usize, 0u8);
                Self::read_buffer(
                    &mut self.cache,
                    &self.paths,
                    instance.file_idx,
                    chunk.position,
                    &mut buffer,
                )?;
                stdout_buf.write_all(&buffer)?;
            }
        }
        Ok(())
    }

    pub fn show(&mut self, opts: &ShowOpts) -> anyhow::Result<()> {
        let job_id_width = if let Some(max_id) = self.index.keys().max() {
            max_id.as_num().checked_ilog10().unwrap_or(0) as usize + 1
        } else {
            return Ok(());
        };
        let task_id_width = if let Some(max_id) = self.index.values().flat_map(|x| x.keys()).max() {
            max_id.as_num().checked_ilog10().unwrap_or(0) as usize + 1
        } else {
            return Ok(());
        };

        let colors = [
            Color::Red,
            Color::Green,
            Color::Yellow,
            Color::Blue,
            Color::Magenta,
            Color::Cyan,
        ];

        let selected_channel_id = opts.channel.as_ref().map(|c| match c {
            Channel::Stdout => 0,
            Channel::Stderr => 1,
        });

        let stdout = std::io::stdout();
        let mut stdout_buf = BufWriter::new(stdout.lock());

        let mut chunks = Vec::new();
        for (job_id, tasks) in self.index.iter() {
            if let Some(selected_job_id) = opts.job {
                if *job_id != selected_job_id {
                    continue;
                }
            }
            for (task_id, task_info) in tasks.iter() {
                let instance = task_info.last_instance();
                for channel_id in 0..2 {
                    if selected_channel_id.map(|x| x == channel_id).unwrap_or(true) {
                        for chunk in &instance.channels[channel_id] {
                            chunks.push((job_id, task_id, instance, channel_id, chunk.clone()))
                        }
                    }
                }
            }
        }
        chunks.sort_by_key(|x| x.4.time);
        let mut buffer = Vec::new();

        for (job_id, task_id, instance, channel_id, chunk) in chunks {
            buffer.resize(chunk.size as usize, 0u8);
            Self::read_buffer(
                &mut self.cache,
                &self.paths,
                instance.file_idx,
                chunk.position,
                &mut buffer,
            )?;
            if buffer.last() != Some(&b'\n') {
                buffer.push(b'\n');
            }
            let color = colors[task_id.as_num() as usize % colors.len()];
            let header =
                format!("{job_id:0job_id_width$}.{task_id:0task_id_width$}:{channel_id}>",);
            write!(
                stdout_buf,
                "{} {}",
                header.on_color(color),
                String::from_utf8_lossy(&buffer)
            )?;
        }
        Ok(())
    }
}
