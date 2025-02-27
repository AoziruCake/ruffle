//! Code relating to executable functions + calling conventions.

use crate::avm1::activation::Activation;
use crate::avm1::error::Error;
use crate::avm1::object::super_object::SuperObject;
use crate::avm1::property::Attribute;
use crate::avm1::scope::Scope;
use crate::avm1::value::Value;
use crate::avm1::{ArrayObject, Object, ObjectPtr, ScriptObject, TObject};
use crate::display_object::{DisplayObject, TDisplayObject};
use crate::string::AvmString;
use crate::tag_utils::SwfSlice;
use gc_arena::{Collect, CollectionContext, Gc, GcCell, MutationContext};
use std::{borrow::Cow, fmt, num::NonZeroU8};
use swf::{avm1::types::FunctionFlags, SwfStr};

/// Represents a function defined in Ruffle's code.
///
/// Parameters are as follows:
///
///  * The AVM1 runtime
///  * The action context
///  * The current `this` object
///  * The arguments this function was called with
///
/// Native functions are allowed to return a value or `None`. `None` indicates
/// that the given value will not be returned on the stack and instead will
/// resolve on the AVM stack, as if you had called a non-native function. If
/// your function yields `None`, you must ensure that the top-most activation
/// in the AVM1 runtime will return with the value of this function.
pub type NativeFunction = for<'gc> fn(
    &mut Activation<'_, 'gc, '_>,
    Object<'gc>,
    &[Value<'gc>],
) -> Result<Value<'gc>, Error<'gc>>;

/// Indicates the reason for an execution
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ExecutionReason {
    /// This execution is a "normal" function call from ActionScript bytecode.
    FunctionCall,

    /// This execution is a "special" internal function call from the player,
    /// such as getters, setters, `toString`, or event handlers.
    Special,
}

/// Represents a function defined in the AVM1 runtime, either through
/// `DefineFunction` or `DefineFunction2`.
#[derive(Debug, Clone, Collect)]
#[collect(no_drop)]
pub struct Avm1Function<'gc> {
    /// The file format version of the SWF that generated this function.
    swf_version: u8,

    /// A reference to the underlying SWF data.
    data: SwfSlice,

    /// The name of the function, if not anonymous.
    name: Option<AvmString<'gc>>,

    /// The number of registers to allocate for this function's private register
    /// set. Any register beyond this ID will be served from the global one.
    register_count: u8,

    /// The parameters of the function.
    params: Vec<Param<'gc>>,

    /// The scope the function was born into.
    scope: GcCell<'gc, Scope<'gc>>,

    /// The constant pool the function executes with.
    constant_pool: GcCell<'gc, Vec<Value<'gc>>>,

    /// The base movie clip that the function was defined on.
    /// This is the movie clip that contains the bytecode.
    base_clip: DisplayObject<'gc>,

    /// The flags that define the preloaded registers of the function.
    #[collect(require_static)]
    flags: FunctionFlags,
}

impl<'gc> Avm1Function<'gc> {
    /// Construct a function from a DefineFunction2 action.
    pub fn from_swf_function(
        gc_context: MutationContext<'gc, '_>,
        swf_version: u8,
        actions: SwfSlice,
        swf_function: swf::avm1::types::DefineFunction2,
        scope: GcCell<'gc, Scope<'gc>>,
        constant_pool: GcCell<'gc, Vec<Value<'gc>>>,
        base_clip: DisplayObject<'gc>,
    ) -> Self {
        let encoding = SwfStr::encoding_for_version(swf_version);
        let name = if swf_function.name.is_empty() {
            None
        } else {
            let name = swf_function.name.to_str_lossy(encoding);
            Some(AvmString::new_utf8(gc_context, name))
        };

        let params = swf_function
            .params
            .iter()
            .map(|p| {
                let name = p.name.to_str_lossy(encoding);
                Param {
                    register: p.register_index,
                    name: AvmString::new_utf8(gc_context, name),
                }
            })
            .collect();

        Avm1Function {
            swf_version,
            data: actions,
            name,
            register_count: swf_function.register_count,
            params,
            scope,
            constant_pool,
            base_clip,
            flags: swf_function.flags,
        }
    }

    pub fn swf_version(&self) -> u8 {
        self.swf_version
    }

    pub fn data(&self) -> SwfSlice {
        self.data.clone()
    }

    pub fn name(&self) -> Option<AvmString<'gc>> {
        self.name
    }

    pub fn scope(&self) -> GcCell<'gc, Scope<'gc>> {
        self.scope
    }

    pub fn register_count(&self) -> u8 {
        self.register_count
    }

    fn debug_string_for_call(&self, name: ExecutionName<'gc>, args: &[Value<'gc>]) -> String {
        let mut result = match self.name.map(ExecutionName::Dynamic).unwrap_or(name) {
            ExecutionName::Static(n) => n.to_owned(),
            ExecutionName::Dynamic(n) => n.to_utf8_lossy().into_owned(),
        };
        result.push('(');
        for i in 0..args.len() {
            result.push_str(args.get(i).unwrap().type_of());
            if i < args.len() - 1 {
                result.push_str(", ");
            }
        }
        result.push(')');
        result
    }

    fn load_this(&self, frame: &mut Activation<'_, 'gc, '_>, this: Value<'gc>, preload_r: &mut u8) {
        let preload = self.flags.contains(FunctionFlags::PRELOAD_THIS);
        let suppress = self.flags.contains(FunctionFlags::SUPPRESS_THIS);

        if preload {
            // The register is set to undefined if both flags are set.
            let this = if suppress { Value::Undefined } else { this };
            frame.set_local_register(*preload_r, this);
            *preload_r += 1;
        }
    }

    fn load_arguments(
        &self,
        frame: &mut Activation<'_, 'gc, '_>,
        args: &[Value<'gc>],
        caller: Option<Object<'gc>>,
        preload_r: &mut u8,
    ) {
        let preload = self.flags.contains(FunctionFlags::PRELOAD_ARGUMENTS);
        let suppress = self.flags.contains(FunctionFlags::SUPPRESS_ARGUMENTS);

        if suppress && !preload {
            return;
        }

        let arguments = ArrayObject::new(
            frame.context.gc_context,
            frame.context.avm1.prototypes().array,
            args.iter().cloned(),
        );

        arguments.define_value(
            frame.context.gc_context,
            "callee",
            frame.callee.unwrap().into(),
            Attribute::DONT_ENUM,
        );

        arguments.define_value(
            frame.context.gc_context,
            "caller",
            caller.map(Value::from).unwrap_or(Value::Null),
            Attribute::DONT_ENUM,
        );

        let arguments = Value::from(arguments);

        // Contrarily to `this` and `super`, setting both flags is equivalent to just setting `preload`.
        if preload {
            frame.set_local_register(*preload_r, arguments);
            *preload_r += 1;
        } else {
            frame.force_define_local("arguments".into(), arguments);
        }
    }

    fn load_super(
        &self,
        frame: &mut Activation<'_, 'gc, '_>,
        this: Option<Object<'gc>>,
        depth: u8,
        preload_r: &mut u8,
    ) {
        let preload = self.flags.contains(FunctionFlags::PRELOAD_SUPER);
        let suppress = self.flags.contains(FunctionFlags::SUPPRESS_SUPER);

        // TODO: `super` should only be defined if this was a method call (depth > 0?)
        // `f[""]()` emits a CallMethod op, causing `this` to be undefined, but `super` is a function; what is it?
        let zuper = this
            .filter(|_| !suppress)
            .map(|this| SuperObject::new(frame, this, depth).into());

        if preload {
            // The register is set to undefined if both flags are set.
            frame.set_local_register(*preload_r, zuper.unwrap_or(Value::Undefined));
            *preload_r += 1;
        } else if let Some(zuper) = zuper {
            frame.force_define_local("super".into(), zuper);
        }
    }

    fn load_root(&self, frame: &mut Activation<'_, 'gc, '_>, preload_r: &mut u8) {
        if self.flags.contains(FunctionFlags::PRELOAD_ROOT) {
            let root = frame.base_clip().avm1_root().object();
            frame.set_local_register(*preload_r, root);
            *preload_r += 1;
        }
    }

    fn load_parent(&self, frame: &mut Activation<'_, 'gc, '_>, preload_r: &mut u8) {
        if self.flags.contains(FunctionFlags::PRELOAD_PARENT) {
            // If _parent is undefined (because this is a root timeline), it actually does not get pushed,
            // and _global ends up incorrectly taking _parent's register.
            // See test for more info.
            if let Some(parent) = frame.base_clip().avm1_parent() {
                frame.set_local_register(*preload_r, parent.object());
                *preload_r += 1;
            }
        }
    }

    fn load_global(&self, frame: &mut Activation<'_, 'gc, '_>, preload_r: &mut u8) {
        if self.flags.contains(FunctionFlags::PRELOAD_GLOBAL) {
            let global = frame.context.avm1.global_object();
            frame.set_local_register(*preload_r, global);
            *preload_r += 1;
        }
    }
}

#[derive(Debug, Clone, Collect)]
#[collect(no_drop)]
struct Param<'gc> {
    /// The register the argument will be preloaded into.
    ///
    /// If `register` is `None`, then this parameter will be stored in a named variable in the
    /// function activation and can be accessed using `GetVariable`/`SetVariable`.
    /// Otherwise, the parameter is loaded into a register and must be accessed with
    /// `Push`/`StoreRegister`.
    #[collect(require_static)]
    register: Option<NonZeroU8>,

    /// The name of the parameter.
    name: AvmString<'gc>,
}

/// Represents a function that can be defined in the Ruffle runtime or by the
/// AVM1 bytecode itself.
#[derive(Clone)]
pub enum Executable<'gc> {
    /// A function provided by the Ruffle runtime and implemented in Rust.
    Native(NativeFunction),

    /// ActionScript data defined by a previous `DefineFunction` or
    /// `DefineFunction2` action.
    Action(Gc<'gc, Avm1Function<'gc>>),
}

unsafe impl<'gc> Collect for Executable<'gc> {
    fn trace(&self, cc: CollectionContext) {
        match self {
            Self::Native(_) => {}
            Self::Action(af) => af.trace(cc),
        }
    }
}

impl fmt::Debug for Executable<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Executable::Native(nf) => f
                .debug_tuple("Executable::Native")
                .field(&format!("{:p}", nf))
                .finish(),
            Executable::Action(af) => f.debug_tuple("Executable::Action").field(&af).finish(),
        }
    }
}

/// Indicates the default name to use for this execution in debug builds.
pub enum ExecutionName<'gc> {
    Static(&'static str),
    Dynamic(AvmString<'gc>),
}

impl<'gc> Executable<'gc> {
    /// Execute the given code.
    ///
    /// Execution is not guaranteed to have completed when this function
    /// returns. If on-stack execution is possible, then this function returns
    /// a return value you must push onto the stack. Otherwise, you must
    /// create a new stack frame and execute the action data yourself.
    #[allow(clippy::too_many_arguments)]
    pub fn exec(
        &self,
        name: ExecutionName<'gc>,
        activation: &mut Activation<'_, 'gc, '_>,
        this: Value<'gc>,
        depth: u8,
        args: &[Value<'gc>],
        reason: ExecutionReason,
        callee: Object<'gc>,
    ) -> Result<Value<'gc>, Error<'gc>> {
        let af = match self {
            Executable::Native(nf) => {
                // TODO: Change NativeFunction to accept `this: Value`.
                let this = this.coerce_to_object(activation);
                return nf(activation, this, args);
            }
            Executable::Action(af) => af,
        };

        let this_obj = match this {
            Value::Object(obj) => Some(obj),
            _ => None,
        };

        let target = activation.target_clip_or_root();
        let is_closure = activation.swf_version() >= 6;
        let base_clip =
            if (is_closure || reason == ExecutionReason::Special) && !af.base_clip.removed() {
                af.base_clip
            } else {
                this_obj
                    .and_then(|this| this.as_display_object())
                    .unwrap_or(target)
            };
        let (swf_version, parent_scope) = if is_closure {
            // Function calls in a v6+ SWF are proper closures, and "close" over the scope that defined the function:
            // * Use the SWF version from the SWF that defined the function.
            // * Use the base clip from when the function was defined.
            // * Close over the scope from when the function was defined.
            (af.swf_version(), af.scope())
        } else {
            // Function calls in a v5 SWF are *not* closures, and will use the settings of
            // `this`, regardless of the function's origin:
            // * Use the SWF version of `this`.
            // * Use the base clip of `this`.
            // * Allocate a new scope using the given base clip. No previous scope is closed over.
            let swf_version = base_clip.swf_version().max(5);
            let base_clip_obj = match base_clip.object() {
                Value::Object(o) => o,
                _ => unreachable!(),
            };
            // TODO: It would be nice to avoid these extra Scope allocs.
            let scope = GcCell::allocate(
                activation.context.gc_context,
                Scope::new(
                    GcCell::allocate(
                        activation.context.gc_context,
                        Scope::from_global_object(activation.context.avm1.global_object_cell()),
                    ),
                    super::scope::ScopeClass::Target,
                    base_clip_obj,
                ),
            );
            (swf_version, scope)
        };

        let child_scope = GcCell::allocate(
            activation.context.gc_context,
            Scope::new_local_scope(parent_scope, activation.context.gc_context),
        );

        // The caller is the previous callee.
        let arguments_caller = activation.callee;

        let name = if cfg!(feature = "avm_debug") {
            Cow::Owned(af.debug_string_for_call(name, args))
        } else {
            Cow::Borrowed("[Anonymous]")
        };

        let is_this_inherited = af
            .flags
            .intersects(FunctionFlags::PRELOAD_THIS | FunctionFlags::SUPPRESS_THIS);
        let local_this = if is_this_inherited {
            activation.this_cell()
        } else {
            this
        };

        let max_recursion_depth = activation.context.avm1.max_recursion_depth();
        let mut frame = Activation::from_action(
            activation.context.reborrow(),
            activation.id.function(name, reason, max_recursion_depth)?,
            swf_version,
            child_scope,
            af.constant_pool,
            base_clip,
            local_this,
            Some(callee),
        );

        frame.allocate_local_registers(af.register_count(), frame.context.gc_context);

        let mut preload_r = 1;
        af.load_this(&mut frame, this, &mut preload_r);
        af.load_arguments(&mut frame, args, arguments_caller, &mut preload_r);
        af.load_super(&mut frame, this_obj, depth, &mut preload_r);
        af.load_root(&mut frame, &mut preload_r);
        af.load_parent(&mut frame, &mut preload_r);
        af.load_global(&mut frame, &mut preload_r);

        // Any unassigned args are set to undefined to prevent assignments from leaking to the parent scope (#2166)
        let args_iter = args
            .iter()
            .cloned()
            .chain(std::iter::repeat(Value::Undefined));

        //TODO: What happens if the argument registers clash with the
        //preloaded registers? What gets done last?
        for (param, value) in af.params.iter().zip(args_iter) {
            if let Some(register) = param.register {
                frame.set_local_register(register.get(), value);
            } else {
                frame.force_define_local(param.name, value);
            }
        }

        Ok(frame.run_actions(af.data.clone())?.value())
    }
}

impl<'gc> From<NativeFunction> for Executable<'gc> {
    fn from(nf: NativeFunction) -> Self {
        Executable::Native(nf)
    }
}

impl<'gc> From<Gc<'gc, Avm1Function<'gc>>> for Executable<'gc> {
    fn from(af: Gc<'gc, Avm1Function<'gc>>) -> Self {
        Executable::Action(af)
    }
}

/// Represents an `Object` that holds executable code.
#[derive(Debug, Clone, Collect, Copy)]
#[collect(no_drop)]
pub struct FunctionObject<'gc> {
    /// The script object base.
    ///
    /// TODO: Can we move the object's data into our own struct?
    base: ScriptObject<'gc>,

    data: GcCell<'gc, FunctionObjectData<'gc>>,
}

#[derive(Debug, Clone, Collect)]
#[collect(no_drop)]
struct FunctionObjectData<'gc> {
    /// The code that will be invoked when this object is called.
    function: Option<Executable<'gc>>,
    /// The code that will be invoked when this object is constructed.
    constructor: Option<Executable<'gc>>,
}

impl<'gc> FunctionObject<'gc> {
    /// Construct a function sans prototype.
    pub fn bare_function(
        gc_context: MutationContext<'gc, '_>,
        function: Option<Executable<'gc>>,
        constructor: Option<Executable<'gc>>,
        fn_proto: Object<'gc>,
    ) -> Self {
        Self {
            base: ScriptObject::new(gc_context, Some(fn_proto)),
            data: GcCell::allocate(
                gc_context,
                FunctionObjectData {
                    function,
                    constructor,
                },
            ),
        }
    }

    /// Construct a function with any combination of regular and constructor parts.
    ///
    /// Since prototypes need to link back to themselves, this function builds
    /// both objects itself and returns the function to you, fully allocated.
    ///
    /// `fn_proto` refers to the implicit proto of the function object, and the
    /// `prototype` refers to the explicit prototype of the function.
    /// The function and its prototype will be linked to each other.
    fn allocate_function(
        gc_context: MutationContext<'gc, '_>,
        function: Option<Executable<'gc>>,
        constructor: Option<Executable<'gc>>,
        fn_proto: Object<'gc>,
        prototype: Object<'gc>,
    ) -> Object<'gc> {
        let function = Self::bare_function(gc_context, function, constructor, fn_proto).into();

        prototype.define_value(
            gc_context,
            "constructor",
            Value::Object(function),
            Attribute::DONT_ENUM,
        );
        function.define_value(
            gc_context,
            "prototype",
            prototype.into(),
            Attribute::empty(),
        );

        function
    }

    /// Construct a regular function from an executable and associated protos.
    pub fn function(
        gc_context: MutationContext<'gc, '_>,
        function: impl Into<Executable<'gc>>,
        fn_proto: Object<'gc>,
        prototype: Object<'gc>,
    ) -> Object<'gc> {
        Self::allocate_function(gc_context, Some(function.into()), None, fn_proto, prototype)
    }

    /// Construct a regular and constructor function from an executable and associated protos.
    pub fn constructor(
        gc_context: MutationContext<'gc, '_>,
        constructor: impl Into<Executable<'gc>>,
        function: impl Into<Executable<'gc>>,
        fn_proto: Object<'gc>,
        prototype: Object<'gc>,
    ) -> Object<'gc> {
        Self::allocate_function(
            gc_context,
            Some(function.into()),
            Some(constructor.into()),
            fn_proto,
            prototype,
        )
    }
}

impl<'gc> TObject<'gc> for FunctionObject<'gc> {
    fn get_local_stored(
        &self,
        name: impl Into<AvmString<'gc>>,
        activation: &mut Activation<'_, 'gc, '_>,
    ) -> Option<Value<'gc>> {
        self.base.get_local_stored(name, activation)
    }

    fn set_local(
        &self,
        name: AvmString<'gc>,
        value: Value<'gc>,
        activation: &mut Activation<'_, 'gc, '_>,
        this: Object<'gc>,
    ) -> Result<(), Error<'gc>> {
        self.base.set_local(name, value, activation, this)
    }

    fn call(
        &self,
        name: AvmString<'gc>,
        activation: &mut Activation<'_, 'gc, '_>,
        this: Value<'gc>,
        args: &[Value<'gc>],
    ) -> Result<Value<'gc>, Error<'gc>> {
        match self.as_executable() {
            Some(exec) => exec.exec(
                ExecutionName::Dynamic(name),
                activation,
                this,
                0,
                args,
                ExecutionReason::FunctionCall,
                (*self).into(),
            ),
            None => Ok(Value::Undefined),
        }
    }

    fn construct_on_existing(
        &self,
        activation: &mut Activation<'_, 'gc, '_>,
        this: Object<'gc>,
        args: &[Value<'gc>],
    ) -> Result<(), Error<'gc>> {
        this.define_value(
            activation.context.gc_context,
            "__constructor__",
            (*self).into(),
            Attribute::DONT_ENUM,
        );
        if activation.swf_version() < 7 {
            this.define_value(
                activation.context.gc_context,
                "constructor",
                (*self).into(),
                Attribute::DONT_ENUM,
            );
        }
        // TODO: de-duplicate code.
        if let Some(exec) = &self.data.read().constructor {
            let _ = exec.exec(
                ExecutionName::Static("[ctor]"),
                activation,
                this.into(),
                1,
                args,
                ExecutionReason::FunctionCall,
                (*self).into(),
            )?;
        } else if let Some(exec) = &self.data.read().function {
            let _ = exec.exec(
                ExecutionName::Static("[ctor]"),
                activation,
                this.into(),
                1,
                args,
                ExecutionReason::FunctionCall,
                (*self).into(),
            )?;
        }
        Ok(())
    }

    fn construct(
        &self,
        activation: &mut Activation<'_, 'gc, '_>,
        args: &[Value<'gc>],
    ) -> Result<Value<'gc>, Error<'gc>> {
        let prototype = self
            .get("prototype", activation)?
            .coerce_to_object(activation);
        let this = prototype.create_bare_object(activation, prototype)?;

        this.define_value(
            activation.context.gc_context,
            "__constructor__",
            (*self).into(),
            Attribute::DONT_ENUM,
        );
        if activation.swf_version() < 7 {
            this.define_value(
                activation.context.gc_context,
                "constructor",
                (*self).into(),
                Attribute::DONT_ENUM,
            );
        }
        // TODO: de-duplicate code.
        if let Some(exec) = &self.data.read().constructor {
            // Native constructors will return the constructed `this`.
            // This allows for `new Object` etc. returning different types.
            let this = exec.exec(
                ExecutionName::Static("[ctor]"),
                activation,
                this.into(),
                1,
                args,
                ExecutionReason::FunctionCall,
                (*self).into(),
            )?;
            Ok(this)
        } else if let Some(exec) = &self.data.read().function {
            let _ = exec.exec(
                ExecutionName::Static("[ctor]"),
                activation,
                this.into(),
                1,
                args,
                ExecutionReason::FunctionCall,
                (*self).into(),
            )?;
            Ok(this.into())
        } else {
            Ok(Value::Undefined)
        }
    }

    fn getter(
        &self,
        name: AvmString<'gc>,
        activation: &mut Activation<'_, 'gc, '_>,
    ) -> Option<Object<'gc>> {
        self.base.getter(name, activation)
    }

    fn setter(
        &self,
        name: AvmString<'gc>,
        activation: &mut Activation<'_, 'gc, '_>,
    ) -> Option<Object<'gc>> {
        self.base.setter(name, activation)
    }

    fn create_bare_object(
        &self,
        activation: &mut Activation<'_, 'gc, '_>,
        prototype: Object<'gc>,
    ) -> Result<Object<'gc>, Error<'gc>> {
        Ok(FunctionObject {
            base: ScriptObject::new(activation.context.gc_context, Some(prototype)),
            data: GcCell::allocate(
                activation.context.gc_context,
                FunctionObjectData {
                    function: None,
                    constructor: None,
                },
            ),
        }
        .into())
    }

    fn delete(&self, activation: &mut Activation<'_, 'gc, '_>, name: AvmString<'gc>) -> bool {
        self.base.delete(activation, name)
    }

    fn proto(&self, activation: &mut Activation<'_, 'gc, '_>) -> Value<'gc> {
        self.base.proto(activation)
    }

    fn define_value(
        &self,
        gc_context: MutationContext<'gc, '_>,
        name: impl Into<AvmString<'gc>>,
        value: Value<'gc>,
        attributes: Attribute,
    ) {
        self.base.define_value(gc_context, name, value, attributes)
    }

    fn set_attributes(
        &self,
        gc_context: MutationContext<'gc, '_>,
        name: Option<AvmString<'gc>>,
        set_attributes: Attribute,
        clear_attributes: Attribute,
    ) {
        self.base
            .set_attributes(gc_context, name, set_attributes, clear_attributes)
    }

    fn add_property(
        &self,
        gc_context: MutationContext<'gc, '_>,
        name: AvmString<'gc>,
        get: Object<'gc>,
        set: Option<Object<'gc>>,
        attributes: Attribute,
    ) {
        self.base
            .add_property(gc_context, name, get, set, attributes)
    }

    fn add_property_with_case(
        &self,
        activation: &mut Activation<'_, 'gc, '_>,
        name: AvmString<'gc>,
        get: Object<'gc>,
        set: Option<Object<'gc>>,
        attributes: Attribute,
    ) {
        self.base
            .add_property_with_case(activation, name, get, set, attributes)
    }

    fn call_watcher(
        &self,
        activation: &mut Activation<'_, 'gc, '_>,
        name: AvmString<'gc>,
        value: &mut Value<'gc>,
        this: Object<'gc>,
    ) -> Result<(), Error<'gc>> {
        self.base.call_watcher(activation, name, value, this)
    }

    fn watch(
        &self,
        activation: &mut Activation<'_, 'gc, '_>,
        name: AvmString<'gc>,
        callback: Object<'gc>,
        user_data: Value<'gc>,
    ) {
        self.base.watch(activation, name, callback, user_data);
    }

    fn unwatch(&self, activation: &mut Activation<'_, 'gc, '_>, name: AvmString<'gc>) -> bool {
        self.base.unwatch(activation, name)
    }

    fn has_property(&self, activation: &mut Activation<'_, 'gc, '_>, name: AvmString<'gc>) -> bool {
        self.base.has_property(activation, name)
    }

    fn has_own_property(
        &self,
        activation: &mut Activation<'_, 'gc, '_>,
        name: AvmString<'gc>,
    ) -> bool {
        self.base.has_own_property(activation, name)
    }

    fn has_own_virtual(
        &self,
        activation: &mut Activation<'_, 'gc, '_>,
        name: AvmString<'gc>,
    ) -> bool {
        self.base.has_own_virtual(activation, name)
    }

    fn is_property_enumerable(
        &self,
        activation: &mut Activation<'_, 'gc, '_>,
        name: AvmString<'gc>,
    ) -> bool {
        self.base.is_property_enumerable(activation, name)
    }

    fn get_keys(&self, activation: &mut Activation<'_, 'gc, '_>) -> Vec<AvmString<'gc>> {
        self.base.get_keys(activation)
    }

    fn interfaces(&self) -> Vec<Object<'gc>> {
        self.base.interfaces()
    }

    /// Set the interface list for this object. (Only useful for prototypes.)
    fn set_interfaces(&self, gc_context: MutationContext<'gc, '_>, iface_list: Vec<Object<'gc>>) {
        self.base.set_interfaces(gc_context, iface_list)
    }

    fn as_script_object(&self) -> Option<ScriptObject<'gc>> {
        Some(self.base)
    }

    fn as_executable(&self) -> Option<Executable<'gc>> {
        self.data.read().function.clone()
    }

    fn as_ptr(&self) -> *const ObjectPtr {
        self.base.as_ptr()
    }

    fn length(&self, activation: &mut Activation<'_, 'gc, '_>) -> Result<i32, Error<'gc>> {
        self.base.length(activation)
    }

    fn set_length(
        &self,
        activation: &mut Activation<'_, 'gc, '_>,
        length: i32,
    ) -> Result<(), Error<'gc>> {
        self.base.set_length(activation, length)
    }

    fn has_element(&self, activation: &mut Activation<'_, 'gc, '_>, index: i32) -> bool {
        self.base.has_element(activation, index)
    }

    fn get_element(&self, activation: &mut Activation<'_, 'gc, '_>, index: i32) -> Value<'gc> {
        self.base.get_element(activation, index)
    }

    fn set_element(
        &self,
        activation: &mut Activation<'_, 'gc, '_>,
        index: i32,
        value: Value<'gc>,
    ) -> Result<(), Error<'gc>> {
        self.base.set_element(activation, index, value)
    }

    fn delete_element(&self, activation: &mut Activation<'_, 'gc, '_>, index: i32) -> bool {
        self.base.delete_element(activation, index)
    }
}

/// Turns a simple built-in constructor into a function that discards
/// the constructor return value.
/// Use with `FunctionObject::constructor` when defining constructor of
/// built-in objects.
#[macro_export]
macro_rules! constructor_to_fn {
    ($f:expr) => {{
        fn _constructor_fn<'gc>(
            activation: &mut $crate::avm1::activation::Activation<'_, 'gc, '_>,
            this: $crate::avm1::Object<'gc>,
            args: &[$crate::avm1::Value<'gc>],
        ) -> Result<$crate::avm1::Value<'gc>, $crate::avm1::error::Error<'gc>> {
            let _ = $f(activation, this, args)?;
            Ok($crate::avm1::Value::Undefined)
        }
        $crate::avm1::function::Executable::Native(_constructor_fn)
    }};
}
