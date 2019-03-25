use super::cmdline::CommandBuilder;
use super::ownedhandle::OwnedHandle;
use super::winsize;
use failure::Error;
use lazy_static::lazy_static;
use shared_library::shared_library;
use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{self, Error as IoError, Result as IoResult};
use std::mem;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::raw::HANDLE;
use std::path::Path;
use std::ptr;
use std::sync::{Arc, Mutex};
use winapi::shared::minwindef::DWORD;
use winapi::shared::winerror::{HRESULT, S_OK};
use winapi::um::fileapi::WriteFile;
use winapi::um::handleapi::*;
use winapi::um::minwinbase::STILL_ACTIVE;
use winapi::um::namedpipeapi::CreatePipe;
use winapi::um::processthreadsapi::*;
use winapi::um::synchapi::WaitForSingleObject;
use winapi::um::winbase::EXTENDED_STARTUPINFO_PRESENT;
use winapi::um::winbase::INFINITE;
use winapi::um::winbase::STARTUPINFOEXW;
use winapi::um::wincon::COORD;

const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x00020016;

#[derive(Debug)]
pub struct Command {
    builder: CommandBuilder,
    input: Option<OwnedHandle>,
    output: Option<OwnedHandle>,
    hpc: Option<HPCON>,
}

impl Command {
    pub fn new<S: AsRef<OsStr>>(program: S) -> Self {
        Self {
            builder: CommandBuilder::new(program),
            input: None,
            output: None,
            hpc: None,
        }
    }

    pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Command {
        self.builder.arg(arg);
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.builder.args(args);
        self
    }

    pub fn env<K, V>(&mut self, key: K, val: V) -> &mut Command
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.builder.env(key, val);
        self
    }

    fn set_pty(&mut self, input: OwnedHandle, output: OwnedHandle, con: HPCON) -> &mut Command {
        self.input.replace(input);
        self.output.replace(output);
        self.hpc.replace(con);
        self
    }

    pub fn spawn(&mut self) -> Result<Child, Error> {
        let mut si: STARTUPINFOEXW = unsafe { mem::zeroed() };
        si.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;

        let mut attrs = ProcThreadAttributeList::with_capacity(1)?;
        attrs.set_pty(*self.hpc.as_ref().unwrap())?;
        si.lpAttributeList = attrs.as_mut_ptr();

        let mut pi: PROCESS_INFORMATION = unsafe { mem::zeroed() };

        let (mut exe, mut cmdline) = self.builder.cmdline()?;
        let cmd_os = OsString::from_wide(&cmdline);
        eprintln!(
            "Running: module: {} {:?}",
            Path::new(&OsString::from_wide(&exe)).display(),
            cmd_os
        );
        let res = unsafe {
            CreateProcessW(
                exe.as_mut_slice().as_mut_ptr(),
                cmdline.as_mut_slice().as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                0,
                EXTENDED_STARTUPINFO_PRESENT,
                ptr::null_mut(), // FIXME: env
                ptr::null_mut(),
                &mut si.StartupInfo,
                &mut pi,
            )
        };
        if res == 0 {
            let err = IoError::last_os_error();
            bail!("CreateProcessW `{:?}` failed: {}", cmd_os, err);
        }

        // Make sure we close out the thread handle so we don't leak it;
        // we do this simply by making it owned
        let _main_thread = OwnedHandle { handle: pi.hThread };
        let proc = OwnedHandle {
            handle: pi.hProcess,
        };

        Ok(Child { proc })
    }
}

struct ProcThreadAttributeList {
    data: Vec<u8>,
}

impl ProcThreadAttributeList {
    pub fn with_capacity(num_attributes: DWORD) -> Result<Self, Error> {
        let mut bytes_required: usize = 0;
        unsafe {
            InitializeProcThreadAttributeList(
                ptr::null_mut(),
                num_attributes,
                0,
                &mut bytes_required,
            )
        };
        let mut data = Vec::with_capacity(bytes_required);
        // We have the right capacity, so force the vec to consider itself
        // that length.  The contents of those bytes will be maintained
        // by the win32 apis used in this impl.
        unsafe { data.set_len(bytes_required) };

        let attr_ptr = data.as_mut_slice().as_mut_ptr() as *mut _;
        let res = unsafe {
            InitializeProcThreadAttributeList(attr_ptr, num_attributes, 0, &mut bytes_required)
        };
        ensure!(
            res != 0,
            "InitializeProcThreadAttributeList failed: {}",
            IoError::last_os_error()
        );
        Ok(Self { data })
    }

    pub fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.data.as_mut_slice().as_mut_ptr() as *mut _
    }

    pub fn set_pty(&mut self, con: HPCON) -> Result<(), Error> {
        let res = unsafe {
            UpdateProcThreadAttribute(
                self.as_mut_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                con,
                mem::size_of::<HPCON>(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        ensure!(
            res != 0,
            "UpdateProcThreadAttribute failed: {}",
            IoError::last_os_error()
        );
        Ok(())
    }
}

impl Drop for ProcThreadAttributeList {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.as_mut_ptr()) };
    }
}

#[derive(Debug)]
pub struct Child {
    proc: OwnedHandle,
}

impl Child {
    pub fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
        let mut status: DWORD = 0;
        let res = unsafe { GetExitCodeProcess(self.proc.handle, &mut status) };
        if res != 0 {
            if status == STILL_ACTIVE {
                Ok(None)
            } else {
                Ok(Some(ExitStatus { status }))
            }
        } else {
            Ok(None)
        }
    }

    pub fn kill(&mut self) -> IoResult<ExitStatus> {
        unsafe {
            TerminateProcess(self.proc.handle, 1);
        }
        self.wait()
    }

    pub fn wait(&mut self) -> IoResult<ExitStatus> {
        if let Ok(Some(status)) = self.try_wait() {
            return Ok(status);
        }
        unsafe {
            WaitForSingleObject(self.proc.handle, INFINITE);
        }
        let mut status: DWORD = 0;
        let res = unsafe { GetExitCodeProcess(self.proc.handle, &mut status) };
        if res != 0 {
            Ok(ExitStatus { status })
        } else {
            Err(IoError::last_os_error())
        }
    }
}

#[derive(Debug)]
pub struct ExitStatus {
    status: DWORD,
}

type HPCON = HANDLE;

shared_library!(ConPtyFuncs,
    pub fn CreatePseudoConsole(
        size: COORD,
        hInput: HANDLE,
        hOutput: HANDLE,
        flags: DWORD,
        hpc: *mut HPCON
    ) -> HRESULT,
    pub fn ResizePseudoConsole(hpc: HPCON, size: COORD) -> HRESULT,
    pub fn ClosePseudoConsole(hpc: HPCON),
);

lazy_static! {
    static ref CONPTY: ConPtyFuncs = ConPtyFuncs::open(Path::new("kernel32.dll")).expect(
        "this system does not support conpty.  Windows 10 October 2018 or newer is required"
    );
}

struct PsuedoCon {
    con: HPCON,
}
unsafe impl Send for PsuedoCon {}
unsafe impl Sync for PsuedoCon {}
impl Drop for PsuedoCon {
    fn drop(&mut self) {
        unsafe { (CONPTY.ClosePseudoConsole)(self.con) };
    }
}
impl PsuedoCon {
    fn new(size: COORD, input: &OwnedHandle, output: &OwnedHandle) -> Result<Self, Error> {
        let mut con: HPCON = INVALID_HANDLE_VALUE;
        let result =
            unsafe { (CONPTY.CreatePseudoConsole)(size, input.handle, output.handle, 0, &mut con) };
        ensure!(
            result == S_OK,
            "failed to create psuedo console: HRESULT {}",
            result
        );
        Ok(Self { con })
    }
    fn resize(&self, size: COORD) -> Result<(), Error> {
        let result = unsafe { (CONPTY.ResizePseudoConsole)(self.con, size) };
        ensure!(
            result == S_OK,
            "failed to resize console to {}x{}: HRESULT: {}",
            size.X,
            size.Y,
            result
        );
        Ok(())
    }
}

struct Inner {
    con: PsuedoCon,
    readable: OwnedHandle,
    writable: OwnedHandle,
    size: winsize,
}

impl Inner {
    pub fn resize(
        &mut self,
        num_rows: u16,
        num_cols: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> Result<(), Error> {
        self.con.resize(COORD {
            X: num_cols as i16,
            Y: num_rows as i16,
        })?;
        self.size = winsize {
            ws_row: num_rows,
            ws_col: num_cols,
            ws_xpixel: pixel_width,
            ws_ypixel: pixel_height,
        };
        Ok(())
    }
}

#[derive(Clone)]
pub struct MasterPty {
    inner: Arc<Mutex<Inner>>,
}

pub struct SlavePty {
    inner: Arc<Mutex<Inner>>,
}

impl MasterPty {
    pub fn resize(
        &self,
        num_rows: u16,
        num_cols: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> Result<(), Error> {
        let mut inner = self.inner.lock().unwrap();
        inner.resize(num_rows, num_cols, pixel_width, pixel_height)
    }

    pub fn get_size(&self) -> Result<winsize, Error> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.size.clone())
    }

    pub fn try_clone_reader(&self) -> Result<Box<std::io::Read + Send>, Error> {
        Ok(Box::new(self.inner.lock().unwrap().readable.try_clone()?))
    }
}

impl io::Write for MasterPty {
    fn write(&mut self, buf: &[u8]) -> Result<usize, io::Error> {
        self.inner.lock().unwrap().writable.write(buf)
    }
    fn flush(&mut self) -> Result<(), io::Error> {
        Ok(())
    }
}

impl SlavePty {
    pub fn spawn_command(self, mut cmd: Command) -> Result<Child, Error> {
        let inner = self.inner.lock().unwrap();
        cmd.set_pty(
            inner.writable.try_clone()?,
            inner.readable.try_clone()?,
            inner.con.con,
        );

        cmd.spawn()
    }
}

fn pipe() -> Result<(OwnedHandle, OwnedHandle), Error> {
    let mut read: HANDLE = INVALID_HANDLE_VALUE;
    let mut write: HANDLE = INVALID_HANDLE_VALUE;
    if unsafe { CreatePipe(&mut read, &mut write, ptr::null_mut(), 0) } == 0 {
        bail!("CreatePipe failed: {}", IoError::last_os_error());
    }
    Ok((OwnedHandle { handle: read }, OwnedHandle { handle: write }))
}

pub fn openpty(
    num_rows: u16,
    num_cols: u16,
    pixel_width: u16,
    pixel_height: u16,
) -> Result<(MasterPty, SlavePty), Error> {
    let (stdin_read, stdin_write) = pipe()?;
    let (stdout_read, stdout_write) = pipe()?;

    let con = PsuedoCon::new(
        COORD {
            X: num_cols as i16,
            Y: num_rows as i16,
        },
        &stdin_read,
        &stdout_write,
    )?;

    let size = winsize {
        ws_row: num_rows,
        ws_col: num_cols,
        ws_xpixel: pixel_width,
        ws_ypixel: pixel_height,
    };

    let master = MasterPty {
        inner: Arc::new(Mutex::new(Inner {
            con,
            readable: stdout_read,
            writable: stdin_write,
            size,
        })),
    };

    let slave = SlavePty {
        inner: master.inner.clone(),
    };

    Ok((master, slave))
}
