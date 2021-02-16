use crate::eserror::EsError;
use crate::esruntimebuilder::EsRuntimeBuilder;
use crate::esscript::EsScript;
use crate::esvalue::EsValueFacade;
use crate::features;
use crate::features::fetch::request::FetchRequest;
use crate::features::fetch::response::FetchResponse;
use crate::quickjs_utils::{functions, objects};
use crate::quickjscontext::QuickJsContext;
use crate::quickjsruntime::QuickJsRuntime;
use crate::utils::single_threaded_event_queue::SingleThreadedEventQueue;
use crate::utils::task_manager::TaskManager;
use libquickjs_sys as q;
use std::future::Future;
use std::rc::Rc;
use std::sync::{Arc, Weak};

lazy_static! {
    /// a static Multithreaded task manager used to run rust ops async and multithreaded ( in at least 2 threads)
    static ref HELPER_TASKS: Arc<TaskManager> = Arc::new(TaskManager::new(std::cmp::max(2, num_cpus::get())));
}

pub type FetchResponseProvider =
    dyn Fn(&FetchRequest) -> Box<dyn FetchResponse + Send> + Send + Sync + 'static;

pub struct EsRuntimeInner {
    pub(crate) event_queue: Arc<SingleThreadedEventQueue>,
    pub(crate) fetch_response_provider: Option<Box<FetchResponseProvider>>,
}

/// EsRuntime is the main public struct representing a JavaScript runtime.
/// You can construct a new EsRuntime by using the [EsRuntimeBuilder] struct
/// # Example
/// ```rust
/// use quickjs_runtime::esruntimebuilder::EsRuntimeBuilder;
/// let rt = EsRuntimeBuilder::new().build();
/// ```

pub struct EsRuntime {
    pub(crate) inner: Arc<EsRuntimeInner>,
}

impl EsRuntimeInner {
    pub fn add_task<C>(&self, task: C)
    where
        C: FnOnce() + Send + 'static,
    {
        let eq_arc = self.event_queue.clone();
        self.event_queue.add_task(move || {
            task();
            eq_arc.add_task_from_worker(|| {
                QuickJsRuntime::do_with(|q_js_rt| {
                    q_js_rt.run_pending_jobs_if_any();
                })
            })
        });
    }

    pub fn exe_task<C, R: Send + 'static>(&self, task: C) -> R
    where
        C: FnOnce() -> R + Send + 'static,
    {
        let eq_arc = self.event_queue.clone();
        self.event_queue.exe_task(move || {
            let res = task();
            eq_arc.add_task_from_worker(|| {
                QuickJsRuntime::do_with(|q_js_rt| {
                    q_js_rt.run_pending_jobs_if_any();
                })
            });
            res
        })
    }

    pub fn async_task<C, R: Send + 'static>(&self, task: C) -> impl Future<Output = R>
    where
        C: FnOnce() -> R + Send + 'static,
    {
        let eq_arc = self.event_queue.clone();
        self.event_queue.async_task(move || {
            let res = task();
            eq_arc.add_task_from_worker(|| {
                QuickJsRuntime::do_with(|q_js_rt| {
                    q_js_rt.run_pending_jobs_if_any();
                })
            });
            res
        })
    }

    pub fn add_to_event_queue<C, R: Send + 'static>(&self, consumer: C) -> impl Future<Output = R>
    where
        C: FnOnce(&QuickJsRuntime) -> R + Send + 'static,
    {
        self.async_task(|| QuickJsRuntime::do_with(consumer))
    }

    pub(crate) fn add_to_event_queue_from_worker<C>(&self, consumer: C)
    where
        C: FnOnce(&QuickJsRuntime) + 'static,
    {
        let eq_arc = self.event_queue.clone();
        self.event_queue.add_task_from_worker(move || {
            QuickJsRuntime::do_with(|q_js_rt| {
                consumer(q_js_rt);
            });
            eq_arc.add_task_from_worker(|| {
                QuickJsRuntime::do_with(|q_js_rt| {
                    q_js_rt.run_pending_jobs_if_any();
                })
            })
        });
    }

    pub fn add_to_event_queue_sync<C, R>(&self, consumer: C) -> R
    where
        C: FnOnce(&QuickJsRuntime) -> R + Send + 'static,
        R: Send + 'static,
    {
        self.exe_task(|| QuickJsRuntime::do_with(consumer))
    }

    pub(crate) fn create_context(&self, id: &str) -> Result<(), EsError> {
        let id = id.to_string();
        self.event_queue
            .exe_task(move || QuickJsRuntime::create_context(id.as_str()))
    }

    pub(crate) fn drop_context(&self, id: &str) {
        let id = id.to_string();
        self.event_queue
            .exe_task(move || QuickJsRuntime::drop_context(id.as_str()))
    }
}

impl EsRuntime {
    pub(crate) fn new(mut builder: EsRuntimeBuilder) -> Arc<Self> {
        let fetch_response_provider =
            std::mem::replace(&mut builder.opt_fetch_response_provider, None);

        let ret = Arc::new(Self {
            inner: Arc::new(EsRuntimeInner {
                event_queue: SingleThreadedEventQueue::new(),
                fetch_response_provider,
            }),
        });

        ret.inner.event_queue.exe_task(|| {
            let rt_ptr = unsafe { q::JS_NewRuntime() };
            let rt = QuickJsRuntime::new(rt_ptr);
            QuickJsRuntime::init_rt_for_current_thread(rt);
        });

        // init ref in q_js_rt
        let rt_ref = ret.clone();
        ret.inner.event_queue.exe_task(move || {
            QuickJsRuntime::do_with_mut(|m_q_js_rt| {
                m_q_js_rt.init_rt_ref(rt_ref);
            })
        });

        // run single job in eventQueue to init thread_local weak<rtref>

        let res = features::init(ret.clone());
        if res.is_err() {
            panic!("could not init features: {}", res.err().unwrap());
        }

        if let Some(interval) = builder.opt_gc_interval {
            let e_ref: Weak<EsRuntime> = Arc::downgrade(&ret);
            std::thread::spawn(move || loop {
                std::thread::sleep(interval);
                if let Some(rt) = e_ref.upgrade() {
                    log::trace!("running gc from gc interval thread");
                    rt.gc_sync();
                } else {
                    break;
                }
            });
        }

        ret.inner.event_queue.exe_task(|| {
            QuickJsRuntime::do_with_mut(|q_js_rt| {
                if builder.opt_module_script_loader.is_some() {
                    q_js_rt.module_script_loader = Some(builder.opt_module_script_loader.unwrap());
                }
                if builder.opt_native_module_loader.is_some() {
                    q_js_rt.native_module_loader = Some(builder.opt_native_module_loader.unwrap());
                }

                if let Some(limit) = builder.opt_memory_limit_bytes {
                    unsafe {
                        q::JS_SetMemoryLimit(q_js_rt.runtime, limit as _);
                    }
                }
                if let Some(threshold) = builder.opt_gc_threshold {
                    unsafe {
                        q::JS_SetGCThreshold(q_js_rt.runtime, threshold as _);
                    }
                }
                if let Some(stack_size) = builder.opt_max_stack_size {
                    unsafe {
                        #[cfg(target_pointer_width = "64")]
                        q::JS_SetMaxStackSize(q_js_rt.runtime, stack_size);
                        #[cfg(target_pointer_width = "32")]
                        {
                            let u32_max: u64 = u32::MAX as u64;
                            let mut stack_size32: u32 = if (stack_size > u32_max) {
                                u32::MAX
                            } else {
                                stack_size as u32
                            };
                            q::JS_SetMaxStackSize(q_js_rt.runtime, stack_size32);
                        }
                    }
                }
            })
        });

        ret
    }

    /// get the number of todos in the event queue
    pub fn get_todo_count(&self) -> usize {
        self.inner.event_queue.todo_count()
    }

    pub fn builder() -> EsRuntimeBuilder {
        EsRuntimeBuilder::new()
    }

    /// this can be used to run a function in the event_queue thread for the QuickJSRuntime
    /// without borrowing the q_js_rt
    pub fn add_task<C>(&self, task: C)
    where
        C: FnOnce() + Send + 'static,
    {
        self.inner.add_task(task);
    }

    /// this can be used to run a function in the event_queue thread for the QuickJSRuntime
    /// without borrowing the q_js_rt
    pub fn exe_task<C, R: Send + 'static>(&self, task: C) -> R
    where
        C: FnOnce() -> R + Send + 'static,
    {
        self.inner.exe_task(task)
    }

    /// Evaluate a script asynchronously
    pub async fn eval(&self, script: EsScript) -> Result<EsValueFacade, EsError> {
        self.add_to_event_queue(|q_js_rt| {
            let q_ctx = q_js_rt.get_main_context();
            let res = q_ctx.eval(script);
            match res {
                Ok(js) => EsValueFacade::from_jsval(q_ctx, &js),
                Err(e) => Err(e),
            }
        })
        .await
    }

    /// Evaluate a script and return the result synchronously
    /// # example
    /// ```rust
    /// use quickjs_runtime::esruntimebuilder::EsRuntimeBuilder;
    /// use quickjs_runtime::esscript::EsScript;
    /// let rt = EsRuntimeBuilder::new().build();
    /// let script = EsScript::new("my_file.es", "(9 * 3);");
    /// let res = rt.eval_sync(script).ok().expect("script failed");
    /// assert_eq!(res.get_i32(), 27);
    /// ```
    pub fn eval_sync(&self, script: EsScript) -> Result<EsValueFacade, EsError> {
        self.add_to_event_queue_sync(move |q_js_rt| {
            let q_ctx = q_js_rt.get_main_context();
            let res = q_ctx.eval(script);
            match res {
                Ok(val_ref) => EsValueFacade::from_jsval(q_ctx, &val_ref),
                Err(e) => Err(e),
            }
        })
    }

    /// run the garbage collector asynchronously
    pub async fn gc(&self) {
        self.add_to_event_queue(|q_js_rt| q_js_rt.gc()).await
    }

    /// run the garbage collector and wait for it to be done
    pub fn gc_sync(&self) {
        self.add_to_event_queue_sync(|q_js_rt| q_js_rt.gc())
    }

    /// call a function in the engine and await the result
    /// # example
    /// ```rust
    /// use quickjs_runtime::esruntimebuilder::EsRuntimeBuilder;
    /// use quickjs_runtime::esscript::EsScript;
    /// use quickjs_runtime::es_args;
    /// use quickjs_runtime::esvalue::EsValueConvertible;
    /// use quickjs_runtime::esvalue::EsValueFacade;
    /// let rt = EsRuntimeBuilder::new().build();
    /// let script = EsScript::new("my_file.es", "this.com = {my: {methodA: function(a, b, someStr, someBool){return a*b;}}};");
    /// rt.eval_sync(script).ok().expect("script failed");
    /// let res = rt.call_function_sync(vec!["com", "my"], "methodA", vec![7i32.to_es_value_facade(), 5i32.to_es_value_facade(), "abc".to_string().to_es_value_facade(), true.to_es_value_facade()]).ok().expect("func failed");
    /// assert_eq!(res.get_i32(), 35);
    /// ```
    pub fn call_function_sync(
        &self,
        namespace: Vec<&'static str>,
        func_name: &str,
        mut arguments: Vec<EsValueFacade>,
    ) -> Result<EsValueFacade, EsError> {
        let func_name_string = func_name.to_string();

        self.add_to_event_queue_sync(move |q_js_rt| {
            let q_ctx = q_js_rt.get_main_context();

            let mut q_args = vec![];
            for arg in &mut arguments {
                q_args.push(arg.as_js_value(q_ctx)?);
            }

            let res = q_ctx.call_function(namespace, func_name_string.as_str(), q_args);
            match res {
                Ok(val_ref) => EsValueFacade::from_jsval(q_ctx, &val_ref),
                Err(e) => Err(e),
            }
        })
    }

    pub fn clone_inner(&self) -> Arc<EsRuntimeInner> {
        self.inner.clone()
    }

    /// call a function in the engine asynchronously
    /// N.B. func_name is not a &str because of https://github.com/rust-lang/rust/issues/56238 (i think)
    /// # example
    /// ```rust
    /// use quickjs_runtime::esruntimebuilder::EsRuntimeBuilder;
    /// use quickjs_runtime::esscript::EsScript;
    /// use quickjs_runtime::esvalue::EsValueConvertible;
    /// let rt = EsRuntimeBuilder::new().build();
    /// let script = EsScript::new("my_file.es", "this.com = {my: {methodA: function(a, b){return a*b;}}};");
    /// rt.eval_sync(script).ok().expect("script failed");
    /// rt.call_function(vec!["com", "my"], "methodA".to_string(), vec![7.to_es_value_facade(), 5.to_es_value_facade()]);
    /// ```
    pub async fn call_function(
        &self,
        namespace: Vec<&'static str>,
        func_name: String,
        mut arguments: Vec<EsValueFacade>,
    ) -> Result<EsValueFacade, EsError> {
        let func_name_string = func_name.to_string();

        self.add_to_event_queue(move |q_js_rt| {
            let q_ctx = q_js_rt.get_main_context();
            let mut q_args = vec![];
            for arg in &mut arguments {
                match arg.as_js_value(q_ctx) {
                    Ok(js_arg) => q_args.push(js_arg),
                    Err(err) => log::error!(
                        "error occurred in async esruntime::call_function closure: {}",
                        err
                    ),
                }
            }

            let res = q_ctx.call_function(namespace, func_name_string.as_str(), q_args);
            match res {
                Ok(js_ref) => EsValueFacade::from_jsval(q_ctx, &js_ref),
                Err(e) => Err(e),
            }
        })
        .await
    }

    /// evaluate a module, you need if you want to compile a script that contains static imports
    /// e.g.
    /// ```javascript
    /// import {util} from 'file.mes';
    /// console.log(util(1, 2, 3));
    /// ```
    /// please note that the module is cached under the absolute path you passed in the EsScript object
    /// and thus you should take care to make the path unique (hence the absolute_ name)
    /// also to use this you need to build the EsRuntime with a module loader closure
    /// # example
    /// ```rust
    /// use quickjs_runtime::esruntimebuilder::EsRuntimeBuilder;
    /// use quickjs_runtime::esscript::EsScript;
    /// use quickjs_runtime::esvalue::EsValueConvertible;
    /// let rt = EsRuntimeBuilder::new().module_script_loader(|q_ctx, relative_file_path, name| {
    ///     // here you should analyze the relative_file_path, this is the absolute path of the script which contains the import statement
    ///     // if this is e.g. '/opt/files/my_module.mes' and the name is 'other_module.mes' then you should return
    ///     // EsScript object with '/opt/files/other_module.mes' as absolute path
    ///     let name = format!("/opt/files/{}", name);
    ///     // this is of course a bad impl, name might for example be '../files/other_module.mes'
    ///     Some(EsScript::new(name.as_str(), "export const util = function(a, b, c){return a+b+c;};"))
    /// }).build();
    /// let script = EsScript::new("/opt/files/my_module.mes", "import {util} from 'other_module.mes';
    /// console.log(util(1, 2, 3));");
    /// rt.eval_module(script);
    /// ```
    pub async fn eval_module(&self, script: EsScript) {
        self.add_to_event_queue(|q_js_rt| {
            let q_ctx = q_js_rt.get_main_context();
            let res = q_ctx.eval_module(script);
            match res {
                Ok(_) => {}
                Err(e) => log::error!("error in async eval {}", e),
            }
        })
        .await
    }

    /// evaluate a module and return result synchronously
    pub fn eval_module_sync(&self, script: EsScript) -> Result<EsValueFacade, EsError> {
        self.add_to_event_queue_sync(move |q_js_rt| {
            let q_ctx = q_js_rt.get_main_context();
            let res = q_ctx.eval_module(script);
            match res {
                Ok(val_ref) => EsValueFacade::from_jsval(q_ctx, &val_ref),
                Err(e) => Err(e),
            }
        })
    }

    /// this is how you add a closure to the worker thread which has an instance of the QuickJsRuntime
    /// this will run asynchronously
    /// # example
    /// ```rust
    /// use quickjs_runtime::esruntimebuilder::EsRuntimeBuilder;
    /// let rt = EsRuntimeBuilder::new().build();
    /// rt.add_to_event_queue(|q_js_rt| {
    ///     // here you are in the worker thread and you can use the quickjs_utils
    ///     q_js_rt.gc();
    /// });
    /// ```
    pub fn add_to_event_queue<C, R: Send + 'static>(&self, consumer: C) -> impl Future<Output = R>
    where
        C: FnOnce(&QuickJsRuntime) -> R + Send + 'static,
    {
        self.inner.add_to_event_queue(consumer)
    }

    /// this is how you add a closure to the worker thread which has an instance of the QuickJsRuntime
    /// this will run and return synchronously
    /// # example
    /// ```rust
    /// use quickjs_runtime::esruntimebuilder::EsRuntimeBuilder;
    /// use quickjs_runtime::esscript::EsScript;
    /// use quickjs_runtime::quickjs_utils::primitives;
    /// let rt = EsRuntimeBuilder::new().build();
    /// let res = rt.add_to_event_queue_sync(|q_js_rt| {
    ///     let q_ctx = q_js_rt.get_main_context();
    ///     // here you are in the worker thread and you can use the quickjs_utils
    ///     let val_ref = q_ctx.eval(EsScript::new("test.es", "(11 * 6);")).ok().expect("script failed");
    ///     primitives::to_i32(&val_ref).ok().expect("could not get i32")
    /// });
    /// assert_eq!(res, 66);
    /// ```
    pub fn add_to_event_queue_sync<C, R>(&self, consumer: C) -> R
    where
        C: FnOnce(&QuickJsRuntime) -> R + Send + 'static,
        R: Send + 'static,
    {
        self.inner.add_to_event_queue_sync(consumer)
    }

    /// this adds a rust function to JavaScript, it is added for all current and future contexts
    /// # Example
    /// ```rust
    /// use quickjs_runtime::esruntimebuilder::EsRuntimeBuilder;
    /// use quickjs_runtime::esscript::EsScript;
    /// use quickjs_runtime::quickjs_utils::primitives;
    /// use quickjs_runtime::esvalue::{EsValueFacade, EsValueConvertible};
    /// let rt = EsRuntimeBuilder::new().build();
    /// rt.set_function(vec!["com", "mycompany", "util"], "methodA", |q_ctx, args: Vec<EsValueFacade>|{
    ///     let a = args[0].get_i32();
    ///     let b = args[1].get_i32();
    ///     Ok((a * b).to_es_value_facade())
    /// });
    /// let res = rt.eval_sync(EsScript::new("test.es", "let a = com.mycompany.util.methodA(13, 17); a * 2;")).ok().expect("script failed");
    /// assert_eq!(res.get_i32(), (13*17*2));
    /// ```
    pub fn set_function<F>(
        &self,
        namespace: Vec<&'static str>,
        name: &str,
        function: F,
    ) -> Result<(), EsError>
    where
        F: Fn(&QuickJsContext, Vec<EsValueFacade>) -> Result<EsValueFacade, EsError>
            + Send
            + 'static,
    {
        let name = name.to_string();
        self.add_to_event_queue_sync(move |q_js_rt| {
            let func_rc = Rc::new(function);
            let name = name.to_string();

            q_js_rt.add_context_init_hook(move |_q_js_rt, q_ctx| {
                let ns = objects::get_namespace_q(q_ctx, namespace.clone(), true)?;

                let func_rc = func_rc.clone();

                let func = functions::new_function_q(
                    q_ctx,
                    name.as_str(),
                    move |q_ctx, _this_ref, args| {
                        let mut args_facades = vec![];

                        for arg_ref in args {
                            args_facades.push(EsValueFacade::from_jsval(q_ctx, &arg_ref)?);
                        }

                        let res = func_rc(q_ctx, args_facades);

                        match res {
                            Ok(mut val_esvf) => val_esvf.as_js_value(q_ctx),
                            Err(e) => Err(e),
                        }
                    },
                    1,
                )?;

                objects::set_property2_q(q_ctx, &ns, name.as_str(), &func, 0)?;

                Ok(())
            })
        })
    }

    /// add a task the the "helper" thread pool
    pub fn add_helper_task<T>(task: T)
    where
        T: FnOnce() + Send + 'static,
    {
        log::trace!("adding a helper task");
        HELPER_TASKS.add_task(task);
    }

    /// create a new context besides the always existing main_context
    /// # todo
    /// EsRuntime needs some more pub methods using context like eval / call_func
    /// # Example
    /// ```
    /// use quickjs_runtime::esruntimebuilder::EsRuntimeBuilder;
    /// use quickjs_runtime::esscript::EsScript;
    /// let rt = EsRuntimeBuilder::new().build();
    /// rt.create_context("my_context");
    /// rt.add_to_event_queue_sync(|q_js_rt| {
    ///    let my_ctx = q_js_rt.get_context("my_context");
    ///    my_ctx.eval(EsScript::new("ctx_test.es", "this.myVar = 'only exists in my_context';"));
    /// });
    /// ```
    pub fn create_context(&self, id: &str) -> Result<(), EsError> {
        self.inner.create_context(id)
    }

    /// drop a context which was created earlier with a call to [create_context()](struct.EsRuntime.html#method.create_context)
    pub fn drop_context(&self, id: &str) {
        self.inner.drop_context(id)
    }
}

#[cfg(test)]
pub mod tests {
    use crate::eserror::EsError;
    use crate::esruntime::EsRuntime;
    use crate::esscript::EsScript;
    use crate::esvalue::{EsValueConvertible, EsValueFacade};
    use crate::quickjs_utils::{primitives, promises};
    use crate::quickjscontext::QuickJsContext;
    use crate::quickjsruntime::NativeModuleLoader;
    use crate::valueref::JSValueRef;
    use futures::executor::block_on;
    use log::debug;
    use log::LevelFilter;
    use std::sync::Arc;
    use std::time::Duration;

    lazy_static! {
        pub static ref TEST_ESRT: Arc<EsRuntime> = init();
    }

    struct TestNativeModuleLoader {}

    impl NativeModuleLoader for TestNativeModuleLoader {
        fn has_module(&self, _q_ctx: &QuickJsContext, module_name: &str) -> bool {
            !module_name.eq("notfound.mes")
        }

        fn get_module_export_names(
            &self,
            _q_ctx: &QuickJsContext,
            _module_name: &str,
        ) -> Vec<String> {
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        }

        fn get_module_exports(
            &self,
            _q_ctx: &QuickJsContext,
            _module_name: &str,
        ) -> Vec<(String, JSValueRef)> {
            vec![
                ("a".to_string(), primitives::from_i32(1234)),
                ("b".to_string(), primitives::from_i32(64834)),
                ("c".to_string(), primitives::from_i32(333)),
            ]
        }
    }

    #[test]
    fn test_rt_drop() {
        let rt = init();
        log::trace!("before drop");

        drop(rt);
        log::trace!("after before drop");
        std::thread::sleep(Duration::from_secs(5));
        log::trace!("after sleep");
    }

    fn init() -> Arc<EsRuntime> {
        simple_logging::log_to_file("esruntime.log", LevelFilter::max())
            .ok()
            .expect("could not init logger");

        log::trace!("TEST_ESRT::init");
        EsRuntime::builder()
            .gc_interval(Duration::from_secs(1))
            .max_stack_size(u64::MAX)
            .module_script_loader(|_q_ctx, _rel, name| {
                if name.eq("notfound.mes") || name.starts_with("greco://") {
                    None
                } else if name.eq("invalid.mes") {
                    Some(EsScript::new(name, "I am the great cornholio! thou'gh shalt&s not p4arse mie!"))
                } else {
                    Some(EsScript::new(name, "export const foo = 'bar';\nexport const mltpl = function(a, b){return a*b;}; globalThis;"))
                }
            })
            .native_module_loader(TestNativeModuleLoader{})
            .build()
    }

    #[test]
    fn test_func() {
        let rt: Arc<EsRuntime> = TEST_ESRT.clone();
        let res = rt.set_function(vec!["nl", "my", "utils"], "methodA", |_q_ctx, args| {
            if args.len() != 2 || !args.get(0).unwrap().is_i32() || !args.get(1).unwrap().is_i32() {
                Err(EsError::new_str(
                    "i'd realy like 2 args of the int32 kind please",
                ))
            } else {
                let a = args.get(0).unwrap().get_i32();
                let b = args.get(1).unwrap().get_i32();
                Ok((a * b).to_es_value_facade())
            }
        });

        match res {
            Ok(_) => {}
            Err(e) => {
                panic!("set_function failed: {}", e);
            }
        }

        let res = rt.eval_sync(EsScript::new(
            "test_func.es",
            "(nl.my.utils.methodA(13, 56));",
        ));

        match res {
            Ok(val) => {
                assert!(val.is_i32());
                assert_eq!(val.get_i32(), 13 * 56);
            }
            Err(e) => {
                panic!("test_func.es failed: {}", e);
            }
        }
    }

    #[test]
    fn test_eval_sync() {
        let rt: Arc<EsRuntime> = TEST_ESRT.clone();
        let res = rt.eval_sync(EsScript::new("test.es", "console.log('foo bar');"));

        match res {
            Ok(_) => {}
            Err(e) => {
                panic!("eval failed: {}", e);
            }
        }

        let res = rt
            .eval_sync(EsScript::new("test.es", "(2 * 7);"))
            .ok()
            .expect("script failed");

        assert_eq!(res.get_i32(), 14);
    }

    #[test]
    fn t1234() {
        // test stack overflow
        let rt: Arc<EsRuntime> = TEST_ESRT.clone();

        rt.add_to_event_queue_sync(|q_js_rt| {
            //q_js_rt.run_pending_jobs_if_any();
            let q_ctx = q_js_rt.get_main_context();
            let r = q_ctx.eval(EsScript::new(
                "test_async.es",
                "let f = async function(){let p = new Promise((resolve, reject) => {resolve(12345);}); const p2 = await p; return p2}; f();",
            )).ok().unwrap();
            log::trace!("tag = {}", r.get_tag());
            //std::thread::sleep(Duration::from_secs(1));

            assert!(promises::is_promise_q(q_ctx, &r));

            if promises::is_promise_q(q_ctx, &r) {
                log::info!("r IS a Promise");
            } else {
                log::error!("r is NOT a Promise");
            }

            std::thread::sleep(Duration::from_secs(1));

            //q_js_rt.run_pending_jobs_if_any();
        });
        rt.add_to_event_queue_sync(|q_js_rt| {
            q_js_rt.run_pending_jobs_if_any();
        });

        std::thread::sleep(Duration::from_secs(1));
    }

    #[test]
    fn test_eval_await() {
        let rt: Arc<EsRuntime> = TEST_ESRT.clone();

        let res = rt.eval_sync(EsScript::new(
            "test_async.es",
            "{let f = async function(){let p = new Promise((resolve, reject) => {resolve(12345);}); const p2 = await p; return p2}; f()};",
        ));

        match res {
            Ok(esvf) => {
                assert!(esvf.is_promise());
                let p_res = esvf
                    .get_promise_result_sync(Duration::from_secs(1))
                    .ok()
                    .expect("prom timed out");
                if p_res.is_err() {
                    panic!("{:?}", p_res.err().unwrap());
                }
                let res = p_res.ok().unwrap();
                assert!(res.is_i32());
                assert_eq!(res.get_i32(), 12345);
            }
            Err(e) => {
                panic!("eval failed: {}", e);
            }
        }
    }

    #[test]
    fn test_promise() {
        let rt: Arc<EsRuntime> = TEST_ESRT.clone();

        let res = rt.eval_sync(EsScript::new(
            "testp2.es",
            "let test_promise_P = (new Promise(function(res, rej) {console.log('before res');res(123);console.log('after res');}).then(function (a) {console.log('prom ressed to ' + a);}).catch(function(x) {console.log('p.ca ex=' + x);}))",
        ));

        match res {
            Ok(_) => {}
            Err(e) => panic!("p script failed: {}", e),
        }
        std::thread::sleep(Duration::from_secs(1));
    }

    #[test]
    fn test_module_sync() {
        log::info!("> test_module_sync");

        let rt = &TEST_ESRT;
        debug!("test static import");
        let res: Result<EsValueFacade, EsError> = rt.eval_module_sync(EsScript::new(
            "test.es",
            "import {foo} from 'test_module.mes';\n console.log('static imp foo = ' + foo);",
        ));

        match res {
            Ok(_) => {
                log::debug!("static import ok");
            }
            Err(e) => {
                log::error!("static import failed: {}", e);
            }
        }

        debug!("test dynamic import");
        let res: Result<EsValueFacade, EsError> = rt.eval_sync(EsScript::new(
            "test_dyn.es",
            "console.log('about to load dynamic module');let dyn_p = import('test_module.mes');dyn_p.then(function (some) {console.log('after dyn');console.log('after dyn ' + typeof some);console.log('mltpl 5, 7 = ' + some.mltpl(5, 7));});dyn_p.catch(function (x) {console.log('imp.cat x=' + x);});console.log('dyn done');",
        ));

        match res {
            Ok(_) => {
                log::debug!("dynamic import ok");
            }
            Err(e) => {
                log::error!("dynamic import failed: {}", e);
            }
        }
        std::thread::sleep(Duration::from_secs(1));

        log::info!("< test_module_sync");
    }

    async fn test_async1() -> i32 {
        let rt = &TEST_ESRT;
        let a = rt.eval(EsScript::new("test_async.es", "122 + 1;")).await;
        a.ok().expect("script failed").get_i32()
    }

    #[test]
    fn test_async() {
        let fut = test_async1();
        let res = block_on(fut);
        assert_eq!(res, 123);
    }

    #[test]
    fn test_macro() {
        let _args = es_args!(1, 2i32, true, "sdf".to_string());
    }
}
