//! Recognises why llama.cpp failed from its log output. The messages differ between backends and
//! releases; matching is on stable fragments, and anything unrecognised stays a generic failure
//! with the log tail shown to the user.

use crate::error::MemoryKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    OutOfMemory(Option<MemoryKind>),
    Backend,
    UnsupportedArchitecture,
    ChatTemplate,
    ModelFile,
}

const DEVICE_MEMORY: &[&str] = &[
    "cudamalloc failed",
    "out of memory",
    "erroroutofdevicememory",
    "outofdevicememory",
    "kiogpucommandbuffercallbackerroroutofmemory",
    "failed to allocate cuda",
    "failed to allocate vulkan",
    "failed to allocate rocm",
    "failed to allocate metal",
    "unable to allocate cuda",
    "unable to allocate vulkan",
    "unable to allocate rocm",
    "unable to allocate metal",
    "hiperroroutofmemory",
];

const HOST_MEMORY: &[&str] = &[
    "std::bad_alloc",
    "cannot allocate memory",
    "failed to allocate cpu",
    "unable to allocate cpu",
];

const BACKEND: &[&str] = &[
    "no cuda-capable device",
    "cuda driver version is insufficient",
    "ggml_cuda_init: failed",
    "failed to initialize cuda",
    "failed to initialize vulkan",
    "ggml_vulkan: no devices found",
    "vk::createinstance: errorincompatibledriver",
    "hiperrornodevice",
    "failed to create metal",
    "error while loading shared libraries",
];

const ARCHITECTURE: &[&str] = &["unknown model architecture", "unsupported model architecture"];
const TEMPLATE: &[&str] = &[
    "failed to parse chat template",
    "chat template parsing error",
    "error: the supplied chat template",
];
const MODEL_FILE: &[&str] = &[
    "gguf_init_from_file",
    "invalid magic",
    "not within the file bounds",
    "failed to open",
    "failed to read",
    "corrupted",
];

/// Removes llama.cpp's elapsed-time and level prefix (`0.01.029.095 E `) from a log line.
pub fn clean(line: &str) -> &str {
    let line = line.trim();
    let mut parts = line.splitn(3, ' ');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(time), Some(level), Some(rest))
            if !time.is_empty()
                && time.chars().all(|c| c.is_ascii_digit() || c == '.')
                && matches!(level, "I" | "W" | "E" | "D") =>
        {
            rest.trim()
        }
        _ => line,
    }
}

fn find(lines: &[String], patterns: &[&str]) -> Option<String> {
    lines
        .iter()
        .find(|l| {
            let l = l.to_ascii_lowercase();
            patterns.iter().any(|p| l.contains(p))
        })
        .map(|l| clean(l).to_owned())
}

/// The most specific failure the log shows, and the line that shows it.
pub fn scan(lines: &[String]) -> Option<(Failure, String)> {
    if let Some(l) = find(lines, HOST_MEMORY) {
        return Some((Failure::OutOfMemory(Some(MemoryKind::Host)), l));
    }
    if let Some(l) = find(lines, DEVICE_MEMORY) {
        let lower = l.to_ascii_lowercase();
        let kind = if lower.contains("cpu") {
            MemoryKind::Host
        } else {
            MemoryKind::Device
        };
        return Some((Failure::OutOfMemory(Some(kind)), l));
    }
    for (patterns, failure) in [
        (BACKEND, Failure::Backend),
        (ARCHITECTURE, Failure::UnsupportedArchitecture),
        (TEMPLATE, Failure::ChatTemplate),
        (MODEL_FILE, Failure::ModelFile),
    ] {
        if let Some(l) = find(lines, patterns) {
            return Some((failure, l));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(str::to_owned).collect()
    }

    #[test]
    fn recognises_memory_failures_per_backend() {
        let cuda = lines(
            "llama_model_load: loading\n\
             ggml_backend_cuda_buffer_type_alloc_buffer: allocating 6144.00 MiB on device 0: cudaMalloc failed: out of memory\n\
             llama_model_load: error loading model: unable to allocate CUDA0 buffer\n\
             llama_model_load_from_file_impl: failed to load model",
        );
        let (f, line) = scan(&cuda).unwrap();
        assert_eq!(f, Failure::OutOfMemory(Some(MemoryKind::Device)));
        assert!(line.contains("cudaMalloc"));
        let vk = lines(
            "ggml_vulkan: Device memory allocation of size 4294967296 failed.\nggml_vulkan: vk::Device::allocateMemory: ErrorOutOfDeviceMemory",
        );
        assert_eq!(
            scan(&vk).unwrap().0,
            Failure::OutOfMemory(Some(MemoryKind::Device))
        );
        let metal = lines(
            "ggml_metal_graph_compute: command buffer 0 failed with status 5\nerror: Insufficient Memory (00000008:kIOGPUCommandBufferCallbackErrorOutOfMemory)",
        );
        assert_eq!(
            scan(&metal).unwrap().0,
            Failure::OutOfMemory(Some(MemoryKind::Device))
        );
        let host = lines("terminate called after throwing an instance of 'std::bad_alloc'");
        assert_eq!(
            scan(&host).unwrap().0,
            Failure::OutOfMemory(Some(MemoryKind::Host))
        );
        let cpu_buf = lines("llama_model_load: error loading model: unable to allocate CPU_Mapped buffer");
        assert_eq!(
            scan(&cpu_buf).unwrap().0,
            Failure::OutOfMemory(Some(MemoryKind::Host))
        );
    }

    #[test]
    fn recognises_backend_model_and_template_failures() {
        let cases = [
            (
                "ggml_cuda_init: failed to initialize CUDA: no CUDA-capable device is detected",
                Failure::Backend,
            ),
            (
                "./llama-server: error while loading shared libraries: libvulkan.so.1",
                Failure::Backend,
            ),
            (
                "llama_model_load: error loading model: error loading model architecture: unknown model architecture: 'gemma9'",
                Failure::UnsupportedArchitecture,
            ),
            (
                "common_chat_templates_init: failed to parse chat template (defaulting to chatml)",
                Failure::ChatTemplate,
            ),
            (
                "gguf_init_from_file_impl: invalid magic characters: 'xxxx'",
                Failure::ModelFile,
            ),
        ];
        for (log, expected) in cases {
            assert_eq!(scan(&lines(log)).map(|f| f.0), Some(expected), "{log}");
        }
        assert_eq!(scan(&lines("main: server is listening")), None);
    }

    #[test]
    fn strips_the_time_and_level_prefix() {
        let l = lines(
            "0.01.029.095 E ggml_backend_cuda_buffer_type_alloc_buffer: cudaMalloc failed: out of memory",
        );
        assert_eq!(
            scan(&l).unwrap().1,
            "ggml_backend_cuda_buffer_type_alloc_buffer: cudaMalloc failed: out of memory"
        );
        assert_eq!(clean("plain line"), "plain line");
    }
}
