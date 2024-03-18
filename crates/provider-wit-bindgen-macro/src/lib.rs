//! Macro for building [wasmCloud capability providers](https://wasmcloud.com/docs/fundamentals/capabilities/create-provider/)
//! from [WIT](https://github.com/WebAssembly/component-model/blob/main/design/mvp/WIT.md) contracts.
//!
//! For example, to build a capability provider for the [wasmcloud:keyvalue contract](https://github.com/wasmCloud/interfaces/tree/main/keyvalue):
//!
//! ```rust,ignore
//! wasmcloud_provider_wit_bindgen::generate!({
//!     impl_struct: KvRedisProvider,
//!     contract: "wasmcloud:keyvalue",
//!     wit_bindgen_cfg: "provider-kvredis"
//! });
//!
//! struct YourProvider;
//! ```
//!
//! All content after `wit_bindgen_cfg: ` is fed to the underlying bindgen (wasmtime::component::macro). In this example, "provider-kvredis" refers to the WIT world that your component will inhabit -- expected to be found at `<project root>/wit/<your world name>.wit`. An example world file:
//!
//! ```rust,ignore
//! package wasmcloud:provider-kvredis
//!
//! world provider-kvredis {
//!     import wasmcloud:keyvalue/key-value
//! }
//! ```
//!
//! For more information on the options available to underlying bindgen, see the [wasmtime-component-bindgen documentation](https://docs.rs/wasmtime/latest/wasmtime/component/macro.bindgen.html).
//!

use std::collections::HashMap;

use anyhow::{bail, Context};
use heck::{ToKebabCase, ToUpperCamelCase};
use proc_macro2::{Ident, Span, TokenStream};
use quote::{quote, ToTokens, TokenStreamExt};
use syn::{
    parse_macro_input, punctuated::Punctuated, visit_mut::VisitMut, ImplItemFn, ItemEnum,
    ItemStruct, ItemType, LitStr, PathSegment, ReturnType, Token,
};
use tracing::debug;
use tracing_subscriber::EnvFilter;
use wit_parser::{Resolve, WorldKey};

mod bindgen_visitor;
use bindgen_visitor::WitBindgenOutputVisitor;

mod config;
use config::ProviderBindgenConfig;

mod rust;

mod vendor;
use vendor::wasmtime_component_macro::bindgen::expand as expand_wasmtime_component;

mod wit;
use wit::{
    translate_export_fn_for_lattice, WitFunctionName, WitInterfacePath, WitNamespaceName,
    WitPackageName,
};

use crate::wit::translate_import_fn_for_lattice;

mod wrpc;

/// Rust module name that is used by wit-bindgen to generate all the modules
const EXPORTS_MODULE_NAME: &str = "exports";

type ImplStructName = String;
type WasmcloudContract = String;

/// Information related to an interface function that will be eventually exposed on the lattice
type LatticeExposedInterface = (WitNamespaceName, WitPackageName, WitFunctionName);

type StructName = String;
type StructLookup = HashMap<StructName, (Punctuated<PathSegment, Token![::]>, ItemStruct)>;

type EnumName = String;
type EnumLookup = HashMap<EnumName, (Punctuated<PathSegment, Token![::]>, ItemEnum)>;

type TypeName = String;
type TypeLookup = HashMap<TypeName, (Punctuated<PathSegment, Token![::]>, ItemType)>;

/// Camel-cased WIT trait name (ex. `WasmcloudKeyvalueKeyValue`)
type WitTraitName = String;

/// Contains information about an method generated by upstream bindgen which represents
/// a method that was exported via WIT.
///
/// This information is essentially "scraped" from the code generated by upstream bindgen.
#[derive(Debug, Clone)]
struct ExportedLatticeMethod {
    /// Name of the operation name that will come in over the lattice
    operation_name: LitStr,

    /// Function name for the Rust method that should be called after a lattice invocation is received
    func_name: Ident,

    /// Invocation arguments (type name & type pair)
    invocation_args: Vec<(Ident, TokenStream)>,

    /// Return type of the invocation
    invocation_return: ReturnType,
}

/// This macro generates functionality necessary to use a WIT-enabled Rust providers (binaries that are managed by the host)
#[proc_macro]
pub fn generate(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init();

    // Parse the provider bindgen macro configuration
    let cfg = parse_macro_input!(input as ProviderBindgenConfig);
    let contract_ident = LitStr::new(&cfg.contract, Span::call_site());

    // Extract the parsed upstream WIT bindgen configuration, which (once successfully parsed)
    // contains metadata extracted from WIT files
    let wit_bindgen_cfg = cfg
        .wit_bindgen_cfg
        .as_ref()
        .context("configuration to pass to WIT bindgen is missing")
        .expect("failed to parse WIT bindgen configuration");

    // Process the parsed WIT metadata to extract imported interface invocation methods & structs,
    // which will be used to generate InvocationHandlers for external calls that the provider may make
    let mut imported_iface_invocation_methods: Vec<TokenStream> = Vec::new();
    for (_, world) in wit_bindgen_cfg.resolve.worlds.iter() {
        for (import_key, _) in world.imports.iter() {
            if let WorldKey::Interface(iface_id) = import_key {
                // Find the parsed interface definition that corresponds to the iface
                let iface = &wit_bindgen_cfg.resolve.interfaces[*iface_id];

                // Some interfaces are known to be *not* coming in from the lattice
                // and should not have invocation handlers generated for them.
                //
                // For example, the wasmcloud:bus interface should not be interpreted
                // as an InvocationHandler generation target
                if iface
                    .package
                    .map(|p| &wit_bindgen_cfg.resolve.packages[p].name)
                    .is_some_and(is_ignored_invocation_handler_pkg)
                {
                    continue;
                }

                // All other interfaces should have their functions processed in order to generate
                // InvocationHandlers in the resulting bindgen output code
                //
                // For each function in an exported interface, we'll need to generate a method
                // on the eventual `InvocationHandler`s that will be built later.
                //
                // Most functions on imported interface to consist of *one* argument which is
                // normally a struct (WIT record type) what represents the information for lattice, ex.:
                //
                //  ```
                //  interface handler {
                //       use types.{some-message}
                //       handle-message: func(msg: some-message) -> result<_, string>
                //   }
                //  ```
                for (iface_fn_name, iface_fn) in iface.functions.iter() {
                    debug!("processing imported interface function: [{iface_fn_name}]");
                    imported_iface_invocation_methods.push(
                        translate_import_fn_for_lattice(iface, iface_fn_name, iface_fn, &cfg)
                            .expect("failed to translate export fn"),
                    );
                }
            }
        }
    }

    // Expand the wasmtime::component macro with the given arguments.
    // We re-use the output of this macro and extract code from it in order to build our own.
    let bindgen_tokens: TokenStream =
        expand_wasmtime_component(wit_bindgen_cfg).unwrap_or_else(syn::Error::into_compile_error);

    // Parse the bindgen-generated tokens into an AST
    // that will be used in the output (combined with other wasmcloud-specific generated code)
    let mut bindgen_ast: syn::File =
        syn::parse2(bindgen_tokens).expect("failed to parse wit-bindgen generated code as file");

    // Traverse the generated upstream wasmtime::component macro output code,
    // to modify it and extract information from it
    let mut visitor = WitBindgenOutputVisitor::new(&cfg);
    visitor.visit_file_mut(&mut bindgen_ast);

    // Turn the function calls extracted from the wasmtime::component macro code
    // into method declarations that enable receiving invocations from the lattice
    let methods_by_iface = build_lattice_methods_by_wit_interface(
        &visitor.serde_extended_structs,
        &visitor.type_lookup,
        &visitor.export_trait_methods,
        &cfg,
    )
    .expect("failed to build lattice methods from WIT interfaces");

    // Create the implementation struct name as an Ident
    let impl_struct_name = Ident::new_raw(cfg.impl_struct.as_str(), Span::call_site());

    // Build a list of match arms for the invocation dispatch that is required
    let mut interface_dispatch_wrpc_match_arms: Vec<TokenStream> = Vec::new();
    let mut iface_tokens = TokenStream::new();

    // Go through every method metadata object (`ExportedLatticeMethod`) extracted from the
    // wasmtime::component macro output code in order to:
    //
    // - Generate struct declarations
    // - Generate traits for each interface (ex. "wasi:keyvalue/eventual" -> `WasiKeyvalueEventual`)
    //
    for (wit_iface_name, methods) in methods_by_iface.iter() {
        // Convert the WIT interface name into an ident
        let wit_iface = Ident::new(wit_iface_name, Span::call_site());

        // Create a list of operation names (ex. `wasmcloud:keyvalue/key-value.get`) that will be
        // used to dispatch incoming provider invocations
        let operation_names = methods
            .clone()
            .into_iter()
            .map(|lm| lm.operation_name)
            .collect::<Vec<LitStr>>();

        // Function names that providers will implement for lattice methods (these functions will be called)
        let func_names = methods
            .clone()
            .into_iter()
            .map(|lm| lm.func_name)
            .collect::<Vec<Ident>>();

        // Gather the invocation args with names, which is either:
        // - all struct members if present
        // - the arg name plus type name for a known type
        // - an empty list for zero args
        let invocation_args_with_types = methods
            .clone()
            .into_iter()
            .map(|lm| {
                let arg_tokens = lm
                    .invocation_args
                    .iter()
                    .map(|(ident, ty)| quote!(#ident: #ty))
                    .collect::<Vec<TokenStream>>();
                quote::quote!(#( #arg_tokens ),*)
            })
            .collect::<Vec<TokenStream>>();

        // Invocation returns of the functions that are called for each lattice method
        let invocation_returns = methods
            .clone()
            .into_iter()
            .map(|lm| lm.invocation_return)
            .collect::<Vec<ReturnType>>();

        // Generate main trait for this interface (ex. `WasiKeyvalueEventual`) that facilitates invocations
        // and pipes through calls to provider impl
        //
        // Create and append the trait for the iface along with
        // the functions that should be implemented by the provider
        iface_tokens.append_all(quote!(
            #[::wasmcloud_provider_wit_bindgen::deps::async_trait::async_trait]
            pub trait #wit_iface {
                fn contract_id() -> &'static str {
                    #contract_ident
                }

                #(
                    async fn #func_names (
                        &self,
                        ctx: ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::Context,
                        #invocation_args_with_types
                    ) #invocation_returns;
                )*
            }
            // END: *Invocation structs & trait for #wit_iface
        ));

        // Build wRPC-compatible match arms that do input parsing and argument expressions, for every method
        // we'll need to build two TokenStreams:
        //
        // - input parsing token stream (i.e. pulling values off)
        // - arguments that go after &self for the function (i.e. the actual args that implementers will use)
        //
        // The token streams generated here will have the same length as lattice methods, and each will correspond 1:1
        let (wrpc_input_parsing_statements, post_self_args, result_encode_tokens) = methods.clone().into_iter().fold(
            (Vec::<TokenStream>::new(), Vec::<TokenStream>::new(), Vec::<TokenStream>::new()),
            |mut acc, lm| {
                // In the case of wRPC, we are going to get a Vec<wprc_transport::Value>, which means we'll have to pull values off one by one
                // and parse them accordingly.
                //
                // We should *not* bundle arguments going over the lattice at all, they'll be individually sent as `wrpc_transport::Value`s
                //
                // All we need to do is insert ctx at the front, and do the rest of it.

                // TODO: REFACTOR -- for WRPC, we should *never* bundle, the args and type names they
                // were supposed to be should always be together

                // Build the code that is going to pull and convert items from the list of params we'll get
                // params are a `Vec<wrpc_transport::Value>`, so we'll need to decode them one by one
                let mut input_decoding_lines = Vec::<TokenStream>::new();

                // todo(vados-cosmonic): we need to encode *and then decode* to get back into the right Rust type...
                // we should be able to improve this and take more straight forward path from Value.
                // (maybe we need to derive ToValue/FromValue) as well for structs/enums
                for (arg_name, arg_type) in lm.invocation_args.iter() {
                    let arg_name_lit = LitStr::new(&arg_name.to_string(), Span::call_site());
                    let arg_ty = arg_type.to_token_stream();
                    input_decoding_lines.push(quote::quote!(
                        let mut #arg_name = ::wasmcloud_provider_wit_bindgen::deps::bytes::BytesMut::new();
                        params
                            .pop()
                            .ok_or_else(|| ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::error::InvocationError::Unexpected(format!("missing expected parameter [{}]", #arg_name_lit)))?
                            .encode(&mut #arg_name)
                            .await
                            .map_err(|e| ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::error::InvocationError::Unexpected(format!("failed to encode parameter [{}]: {e}", #arg_name_lit)))?;
                        let (#arg_name, _): (#arg_ty, _) = ::wasmcloud_provider_wit_bindgen::deps::wrpc_transport::Receive::receive::<::wasmcloud_provider_wit_bindgen::deps::wrpc_transport::DemuxStream>(#arg_name, &mut ::wasmcloud_provider_wit_bindgen::deps::futures::stream::empty(), None)
                            .await
                            .map_err(|e| ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::error::InvocationError::Unexpected(format!("failed to receive parameter [{}]: {e}", #arg_name_lit)))?;
                    ));
                }
                acc.0.push(quote::quote!(#( #input_decoding_lines );*));

                // Build the list of tokens that we'll need for the provider-internal function arguments, after '&self'
                // ex. fn some_fn(&self, ctx, <arg1>, <arg2> ...)
                let arg_idents = vec![Ident::new("ctx", Span::call_site())]
                    .into_iter()
                    .chain(lm.invocation_args.iter().map(|(name, _)| name.clone()))
                    .collect::<Vec<Ident>>();
                acc.1.push(quote!(#( #arg_idents ),*));

                // Build the tokens that we'll need to encode the result. These differ whether we're dealing with a normal type
                // or a special case (i.e. Vec<T> and Option<T>)
                acc.2.push(match lm.invocation_return {
                    syn::ReturnType::Type(_, _) => {
                        quote!(result
                               .encode(&mut res)
                               .await
                               .map_err(|e| {
                                   ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::error::InvocationError::Unexpected(
                                       format!("failed to encode result of operation [{operation}]: {e}")
                                   )
                               })?)
                    }

                    // If we don't parse a complex type we may have gotten a builtin like a `bool` or `u32`, we can pass those through normally
                    syn::ReturnType::Default => {
                        quote!(result
                               .encode(&mut res)
                               .await
                               .map_err(|e| {
                                   ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::error::InvocationError::Unexpected(
                                       format!("failed to encode result of operation [{operation}]: {e}")
                                   )
                               })?)
                    },
                });

                acc
            },
    );

        interface_dispatch_wrpc_match_arms.push(quote!(
            #(
                operation @ #operation_names => {
                    #wrpc_input_parsing_statements
                    let result = #wit_iface::#func_names(
                        self,
                        #post_self_args
                    )
                        .await;

                    let mut res = ::wasmcloud_provider_wit_bindgen::deps::bytes::BytesMut::new();
                    #result_encode_tokens;
                    Ok(res.to_vec())
                }
            )*
        ));
    }

    // Build a list of types that should be included in the output code
    let types: Vec<TokenStream> = visitor
        .type_lookup
        .iter()
        .filter_map(|(_, (_, ty))| {
            // If the name of the type is identical to a bindgen-produced struct or enum that will
            // be added later, this was likely a type alias -- we won't need it
            if visitor
                .serde_extended_structs
                .contains_key(&ty.ident.to_string())
                || visitor
                    .serde_extended_enums
                    .contains_key(&ty.ident.to_string())
            {
                None
            } else {
                Some(ty.to_token_stream())
            }
        })
        .collect();

    // Build a list of structs that should be included
    let structs: Vec<TokenStream> = visitor
        .serde_extended_structs
        .iter()
        .map(|(_, (_, s))| s.to_token_stream())
        .collect();

    // Build a list of enums that should be included
    let enums: Vec<TokenStream> = visitor
        .serde_extended_enums
        .iter()
        .map(|(_, (_, s))| s.to_token_stream())
        .collect();

    // Build mapping of of exports (all exports) to use, only if wrpc feature flag is enabled
    let wrpc_impl_tokens = build_wrpc_impls(&impl_struct_name, &wit_bindgen_cfg.resolve)
        .expect("failed to build provider-sdk wrpc implementation");

    // Build the final chunk of code
    let tokens = quote!(
        // START: per-interface codegen
        #iface_tokens
        // END: per-interface codegen

        // START: wit-bindgen generated types
        #(
            #types
        )*
        // END: wit-bindgen generated types

        // START: wit-bindgen generated structs
        #(
            #structs
        )*
        // END: wit-bindgen generated structs

        // START: wit-bindgen generated enums
        #(
            #enums
        )*
        // END: wit-bindgen generated enums

        // START: general provider

        /// This trait categorizes all wasmCloud lattice compatible providers.
        ///
        /// It is a mirror of ProviderHandler for the purposes of ensuring that
        /// at least the following members are is supported.
        #[::wasmcloud_provider_wit_bindgen::deps::async_trait::async_trait]
        trait WasmcloudCapabilityProvider {
            async fn receive_link_config_as_source(
                &self,
                link_config: impl ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::LinkConfig
            ) -> ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::ProviderOperationResult<()> {
                Ok(())
            }

            async fn receive_link_config_as_target(
                &self,
                link_config: impl ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::LinkConfig
            ) -> ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::ProviderOperationResult<()> {
                Ok(())
            }

            async fn delete_link(
                &self,
                actor_id: &str
            ) -> ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::ProviderOperationResult<()> {
                Ok(())
            }

            async fn shutdown(&self) -> ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::ProviderOperationResult<()> {
                Ok(())
            }
        }

        /// ProviderHandler ensures that your provider handles the basic
        /// required functionality of all Providers on a wasmCloud lattice.
        ///
        /// This implementation is a stub and must be filled out by implementers
        #[::wasmcloud_provider_wit_bindgen::deps::async_trait::async_trait]
        impl ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::ProviderHandler for #impl_struct_name {
            async fn receive_link_config_as_source(
                &self,
                link_config: impl ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::LinkConfig
            ) -> ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::ProviderOperationResult<()> {
                WasmcloudCapabilityProvider::receive_link_config_as_source(self, link_config).await
            }

            async fn receive_link_config_as_target(
                &self,
                link_config: impl ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::LinkConfig
            ) -> ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::ProviderOperationResult<()> {
                WasmcloudCapabilityProvider::receive_link_config_as_target(self, link_config).await

            }

            async fn delete_link(
                &self,
                actor_id: &str
            ) -> ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::ProviderOperationResult<()> {
                WasmcloudCapabilityProvider::delete_link(self, actor_id).await
            }

            async fn shutdown(&self) -> ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::ProviderOperationResult<()> {
                WasmcloudCapabilityProvider::shutdown(self).await
            }
        }

        /// Given the implementation of ProviderHandler and MessageDispatch,
        /// the implementation for your struct is a guaranteed
        impl ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::Provider for #impl_struct_name {}

        /// This handler serves to be used for individual invocations of the actor
        /// as performed by the host runtime
        ///
        /// Interfaces imported by the provider can use this to send traffic across the lattice
        pub struct InvocationHandler {
            wrpc_client: ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::core::wrpc::Client
        }

        impl InvocationHandler {
            pub fn new(target: ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::core::ComponentId) -> Self {
                let connection = ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::get_connection();
                // NOTE: The link definition that is used here is likely (source_id=?, target=<provider>)
                //
                // todo(vados-cosmonic): This invocation handler should arguably create a uni-directional
                // "return" link to anything it wants to call, since links are uni-directional now.
                Self { wrpc_client: connection.get_wrpc_client(&target) }
            }

            #(
                #imported_iface_invocation_methods
            )*
        }

        #wrpc_impl_tokens

        #[::wasmcloud_provider_wit_bindgen::deps::async_trait::async_trait]
        impl ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::WrpcDispatch for #impl_struct_name {
            async fn dispatch_wrpc_dynamic<'a>(
                &'a self,
                ctx: ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::Context,
                operation: String,
                mut params: Vec<::wasmcloud_provider_wit_bindgen::deps::wrpc_transport::Value>,
            ) -> ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::error::InvocationResult<Vec<u8>> {
                use ::wasmcloud_provider_wit_bindgen::deps::wrpc_transport::{Encode, Receive};
                use ::wasmcloud_provider_wit_bindgen::deps::anyhow::Context as _;
                match operation.as_str() {
                    #(
                        #interface_dispatch_wrpc_match_arms
                    )*
                    _ => Err(::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::error::InvocationError::Malformed(format!(
                        "Invalid operation name [{operation}]"
                    )).into())
                }
            }
        }
    );

    tokens.into()
}

/// Build [`ExportedLatticeMethod`]s (including related information to facilitate invocations)
/// for the imports of a WIT interface
fn build_lattice_methods_by_wit_interface(
    struct_lookup: &StructLookup,
    type_lookup: &TypeLookup,
    export_trait_methods: &HashMap<WitInterfacePath, Vec<ImplItemFn>>,
    bindgen_cfg: &ProviderBindgenConfig,
) -> anyhow::Result<HashMap<WitTraitName, Vec<ExportedLatticeMethod>>> {
    let mut methods_by_name: HashMap<WitInterfacePath, Vec<ExportedLatticeMethod>> = HashMap::new();

    // For every trait item generated by an imported WIT interface we must generate the appropriate
    // structures that are expected from incoming messages on the lattice.
    for (wit_iface_name, funcs) in export_trait_methods.iter() {
        for trait_method in funcs.iter() {
            // Rebuild the fully-qualified WIT operation name
            let wit_operation = match wit_iface_name.split('.').collect::<Vec<&str>>()[..] {
                [wit_ns, wit_pkg, iface] => {
                    format!(
                        "{}:{}/{}.{}",
                        wit_ns.to_kebab_case(),
                        wit_pkg.to_kebab_case(),
                        iface.to_kebab_case(),
                        trait_method.sig.ident.to_string().to_kebab_case()
                    )
                }
                _ => bail!("unexpected interface path, expected 3 components"),
            };
            let operation_name = LitStr::new(&wit_operation, trait_method.sig.ident.span());

            // Convert the trait method to code that can be used on the lattice
            let lattice_method = translate_export_fn_for_lattice(
                bindgen_cfg,
                operation_name,
                trait_method,
                struct_lookup,
                type_lookup,
            )?;

            // Convert the iface path into an upper camel case representation, for future conversions to use
            let wit_iface_upper_camel = wit_iface_name
                .split('.')
                .map(|v| v.to_upper_camel_case())
                .collect::<String>();

            // Add the struct and its members to a list that will be used in another quote
            // it cannot be added directly/composed to a TokenStream here to avoid import conflicts
            // in case bindgen-defined types are used.
            methods_by_name
                .entry(wit_iface_upper_camel)
                .or_default()
                .push(lattice_method);
        }
    }
    Ok(methods_by_name)
}

/// Check whether a package should *not* be processed while generating `InvocationHandler`s
fn is_ignored_invocation_handler_pkg(pkg: &wit_parser::PackageName) -> bool {
    matches!(
        (pkg.namespace.as_ref(), pkg.name.as_ref()),
        ("wasmcloud", "bus") | ("wasi", "io")
    )
}

/// Build wRPC implementations needed by the provider, primarily `wasmcloud_provider_sdk::WitRpc`
fn build_wrpc_impls(impl_struct_name: &Ident, resolve: &Resolve) -> anyhow::Result<TokenStream> {
    let mapping = crate::wrpc::generate_wrpc_nats_subject_to_fn_mapping(resolve)
        .context("failed to generate wrpc NATS subject mappings")?;

    // Process `WrpcExport` objects into statements that use the incoming lattice_name
    // and wRPC version for map inserts to build the lookup that should be returned
    let mut insertion_lines: Vec<TokenStream> = Vec::new();
    for crate::wrpc::WrpcExport {
        wit_ns,
        wit_pkg,
        wit_iface,
        wit_iface_fn,
        types,
    } in mapping.into_iter()
    {
        let wit_ns = LitStr::new(&wit_ns, Span::call_site());
        let wit_pkg = LitStr::new(&wit_pkg, Span::call_site());
        let wit_iface = LitStr::new(&wit_iface, Span::call_site());
        let wit_iface_fn = LitStr::new(&wit_iface_fn, Span::call_site());
        let world_key_name = LitStr::new(&types.0, Span::call_site());
        let function_name = LitStr::new(&types.1, Span::call_site());
        let dynamic_fn = LitStr::new(
            &serde_json::to_string::<wrpc_types::DynamicFunction>(&types.2).context("failed to deserialize dynamic function with world_key_name [{world_key_name}],  function name [{function_name}]")?,
            Span::call_site(),
        );

        insertion_lines.push(quote!(
            mapping.insert(
                format!("{lattice_name}.{component_id}.wrpc.{wrpc_version}.{}:{}/{}.{}", #wit_ns, #wit_pkg, #wit_iface, #wit_iface_fn),
                (#world_key_name.into(), #function_name.into(), ::wasmcloud_provider_wit_bindgen::deps::serde_json::from_slice::<::wasmcloud_provider_wit_bindgen::deps::wrpc_types::DynamicFunction>(#dynamic_fn.as_bytes()).expect("failed to deserialize DynamicFunction")),
            );
        ));
    }

    // Build the trait impl
    let tokens = quote!(
        use ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::{WrpcNats, WrpcNatsSubject, WorldKeyName, WitFunction};
        use ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::error::ProviderInitResult;

        #[::wasmcloud_provider_wit_bindgen::deps::async_trait::async_trait]
        impl WrpcNats for #impl_struct_name {
            async fn incoming_wrpc_invocations_by_subject(
                &self,
                lattice_name: impl AsRef<str> + Send,
                component_id: impl AsRef<str> + Send,
                wrpc_version: impl AsRef<str> + Send,
            ) -> ProviderInitResult<
                ::std::collections::HashMap<WrpcNatsSubject, (WorldKeyName, WitFunction, ::wasmcloud_provider_wit_bindgen::deps::wrpc_types::DynamicFunction)>
            > {
                let lattice_name = lattice_name.as_ref();
                let wrpc_version = wrpc_version.as_ref();
                let component_id = component_id.as_ref();
                let mut mapping = ::std::collections::HashMap::new();
                #(
                    #insertion_lines
                )*
                Ok(mapping)
            }
        }
    );

    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use anyhow::Context;
    use proc_macro2::{Span, TokenTree};
    use quote::quote;
    use syn::{parse_quote, ImplItemFn, LitStr, ReturnType};

    use crate::wit::{extract_witified_map, translate_export_fn_for_lattice};
    use crate::ProviderBindgenConfig;

    /// Token trees that we expect to parse into WIT-ified maps should parse
    #[test]
    fn parse_witified_map_type() -> anyhow::Result<()> {
        extract_witified_map(
            &quote!(Vec<(String, String)>)
                .into_iter()
                .collect::<Vec<TokenTree>>(),
        )
        .context("failed to parse WIT-ified map type Vec<(String, String)>")?;
        Ok(())
    }

    /// Ensure WIT-ified maps parse correctly in functions
    #[test]
    fn parse_witified_map_in_fn() -> anyhow::Result<()> {
        let trait_fn: ImplItemFn = parse_quote!(
            fn baz(test_map: Vec<(String, String)>) {}
        );
        let bindgen_cfg = ProviderBindgenConfig {
            impl_struct: "None".into(),
            contract: "wasmcloud:test".into(),
            wit_ns: Some("test".into()),
            wit_pkg: Some("foo".into()),
            exposed_interface_allow_list: Default::default(),
            exposed_interface_deny_list: Default::default(),
            wit_bindgen_cfg: None, // We won't actually run bindgen
            replace_witified_maps: true,
        };
        let operation_name = LitStr::new("wasmcloud:test/test.foo", Span::call_site());
        let lm = translate_export_fn_for_lattice(
            &bindgen_cfg,
            operation_name.clone(),
            &trait_fn,
            &HashMap::new(), // structs
            &HashMap::new(), // types
        )?;

        assert_eq!(lm.operation_name, operation_name);
        assert_eq!(lm.invocation_args.len(), 1);
        assert_eq!(
            lm.invocation_return,
            syn::parse2::<ReturnType>(quote::quote!())?
        );

        Ok(())
    }
}
