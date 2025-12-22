use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, DeriveInput, Fields, ItemImpl, ImplItem, ImplItemFn, FnArg, ReturnType, Pat};

/// Marker attribute to opt a method into span propagation.
/// Use on methods within a `#[service]` impl block.
///
/// ```ignore
/// #[service]
/// impl MyService {
///     #[traced]
///     pub async fn important_operation(&self) -> String {
///         // This method will propagate the caller's tracing span
///     }
/// }
/// ```
#[proc_macro_attribute]
pub fn traced(_attr: TokenStream, item: TokenStream) -> TokenStream {
    // This is just a marker - the actual processing happens in #[service]
    item
}

/// Marker attribute to opt a method out of span propagation.
/// Use on methods within a `#[service(traced)]` impl block.
///
/// ```ignore
/// #[service(traced)]
/// impl MyService {
///     #[untraced]
///     pub async fn cache_refresh(&self) {
///         // This method will NOT propagate spans (no overhead)
///     }
/// }
/// ```
#[proc_macro_attribute]
pub fn untraced(_attr: TokenStream, item: TokenStream) -> TokenStream {
    // This is just a marker - the actual processing happens in #[service]
    item
}

/// Derive macro for Service structs - generates ServiceFactory implementation
///
/// ```ignore
/// #[derive(Service)]
/// struct PaymentService {
///     config: Res<Config>,
///     orders: Client<OrderService>,
/// }
/// ```
///
/// Generates:
/// ```ignore
/// impl ::archy::ServiceFactory for PaymentService {
///     fn create(app: &::archy::App) -> Self {
///         PaymentService {
///             config: app.extract(),
///             orders: app.extract(),
///         }
///     }
/// }
/// ```
#[proc_macro_derive(Service)]
pub fn derive_service(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let fields = match &input.data {
        syn::Data::Struct(data) => match &data.fields {
            Fields::Named(fields) => &fields.named,
            _ => return syn::Error::new_spanned(&input, "#[derive(Service)] only supports structs with named fields")
                .to_compile_error()
                .into(),
        },
        _ => return syn::Error::new_spanned(&input, "#[derive(Service)] only supports structs")
            .to_compile_error()
            .into(),
    };

    let field_inits = fields.iter().map(|f| {
        let field_name = f.ident.as_ref().unwrap();
        quote! { #field_name: app.extract() }
    });

    let expanded = quote! {
        impl ::archy::ServiceFactory for #name {
            fn create(app: &::archy::App) -> Self {
                #name {
                    #(#field_inits),*
                }
            }
        }
    };

    TokenStream::from(expanded)
}

/// Attribute macro for Service impl blocks - generates message enum, Service impl, and Client methods
///
/// # Basic usage
/// ```ignore
/// #[service]
/// impl PaymentService {
///     pub async fn process(&self, amount: u32) -> String {
///         // ...
///     }
/// }
/// ```
///
/// # Tracing support
/// Use `#[service(traced)]` to propagate tracing spans across service calls:
/// ```ignore
/// #[service(traced)]
/// impl PaymentService {
///     pub async fn process(&self, amount: u32) -> String {
///         tracing::info!("Processing"); // inherits caller's span context
///     }
///
///     #[untraced]  // opt-out for this method
///     pub async fn cache_refresh(&self) { ... }
/// }
/// ```
///
/// For non-traced services, individual methods can opt-in:
/// ```ignore
/// #[service]
/// impl CleanupWorker {
///     #[traced]  // opt-in just this method
///     pub async fn handle_request(&self, id: u64) -> String { ... }
/// }
/// ```
///
/// Generates:
/// - Message enum with variants for each public async method
/// - impl Service for PaymentService
/// - Client methods struct with async methods
#[proc_macro_attribute]
pub fn service(attr: TokenStream, item: TokenStream) -> TokenStream {
    // Parse the traced option from #[service] or #[service(traced)]
    let service_traced = if attr.is_empty() {
        false
    } else {
        match syn::parse::<syn::Ident>(attr.clone()) {
            Ok(ident) if ident == "traced" => true,
            Ok(ident) => return syn::Error::new(
                ident.span(),
                format!("expected `traced`, found `{}`", ident)
            ).to_compile_error().into(),
            Err(e) => return e.to_compile_error().into(),
        }
    };

    let input = parse_macro_input!(item as ItemImpl);
    let service_name = match &*input.self_ty {
        syn::Type::Path(type_path) => type_path.path.segments.last().unwrap().ident.clone(),
        _ => return syn::Error::new_spanned(&input.self_ty, "#[service] must be applied to an impl block for a named type")
            .to_compile_error()
            .into(),
    };

    let msg_enum_name = format_ident!("{}Msg", service_name);
    let methods_struct_name = format_ident!("{}Methods", service_name);

    // Collect public async methods with their tracing status
    let mut methods: Vec<(&ImplItemFn, bool)> = Vec::new();
    for item in &input.items {
        if let ImplItem::Fn(method) = item {
            let is_pub = matches!(method.vis, syn::Visibility::Public(_));
            let is_async = method.sig.asyncness.is_some();
            let has_self = method.sig.inputs.first().map_or(false, |arg| matches!(arg, FnArg::Receiver(_)));

            if is_pub && is_async && has_self {
                // Check for #[traced] and #[untraced] attributes on the method
                let has_traced = has_attribute(&method.attrs, "traced");
                let has_untraced = has_attribute(&method.attrs, "untraced");

                // Error if both attributes are present
                if has_traced && has_untraced {
                    return syn::Error::new_spanned(
                        &method.sig.ident,
                        "method cannot have both #[traced] and #[untraced] attributes"
                    ).to_compile_error().into();
                }

                // Determine final traced status:
                // - #[untraced] → not traced
                // - #[traced] → traced
                // - neither → use service default
                let method_traced = if has_untraced {
                    false
                } else if has_traced {
                    true
                } else {
                    service_traced
                };

                methods.push((method, method_traced));
            }
        }
    }

    // Generate message enum variants
    let msg_variants = methods.iter().map(|(method, traced)| {
        let method_name = &method.sig.ident;
        let variant_name = to_pascal_case(&method_name.to_string());
        let variant_ident = format_ident!("{}", variant_name);

        // Get parameters (skip &self)
        let params: Vec<_> = method.sig.inputs.iter().skip(1).filter_map(|arg| {
            if let FnArg::Typed(pat_type) = arg {
                if let Pat::Ident(pat_ident) = &*pat_type.pat {
                    let name = &pat_ident.ident;
                    let ty = &pat_type.ty;
                    return Some(quote! { #name: #ty });
                }
            }
            None
        }).collect();

        // Add span field for traced methods
        let span_field = if *traced {
            quote! { span: ::archy::tracing::Span, }
        } else {
            quote! {}
        };

        // Fire-and-forget optimization: skip respond field for unit returns
        if is_unit_return(&method.sig.output) {
            if params.is_empty() && !*traced {
                quote! { #variant_ident }
            } else {
                quote! { #variant_ident { #(#params,)* #span_field } }
            }
        } else {
            let return_type = match &method.sig.output {
                ReturnType::Default => quote! { () },
                ReturnType::Type(_, ty) => quote! { #ty },
            };
            quote! {
                #variant_ident { #(#params,)* #span_field respond: ::archy::tokio::sync::oneshot::Sender<#return_type> }
            }
        }
    });

    // Generate handle match arms
    let match_arms = methods.iter().map(|(method, traced)| {
        let method_name = &method.sig.ident;
        let variant_name = to_pascal_case(&method_name.to_string());
        let variant_ident = format_ident!("{}", variant_name);

        // Get parameter names (skip &self)
        let param_names: Vec<_> = method.sig.inputs.iter().skip(1).filter_map(|arg| {
            if let FnArg::Typed(pat_type) = arg {
                if let Pat::Ident(pat_ident) = &*pat_type.pat {
                    return Some(&pat_ident.ident);
                }
            }
            None
        }).collect();

        let method_call = if param_names.is_empty() {
            quote! { self.#method_name().await }
        } else {
            quote! { self.#method_name(#(#param_names),*).await }
        };

        // Fire-and-forget optimization: no respond for unit returns
        if is_unit_return(&method.sig.output) {
            let span_pattern = if *traced { quote! { span, } } else { quote! {} };
            let param_pattern = if param_names.is_empty() {
                quote! { #span_pattern }
            } else {
                quote! { #(#param_names,)* #span_pattern }
            };

            if *traced {
                quote! {
                    #msg_enum_name::#variant_ident { #param_pattern } => {
                        ::archy::tracing::Instrument::instrument(async {
                            #method_call;
                        }, span).await
                    }
                }
            } else {
                quote! {
                    #msg_enum_name::#variant_ident { #param_pattern } => {
                        #method_call;
                    }
                }
            }
        } else {
            let span_pattern = if *traced { quote! { span, } } else { quote! {} };
            let param_pattern = if param_names.is_empty() {
                quote! { #span_pattern respond }
            } else {
                quote! { #(#param_names,)* #span_pattern respond }
            };

            if *traced {
                quote! {
                    #msg_enum_name::#variant_ident { #param_pattern } => {
                        ::archy::tracing::Instrument::instrument(async {
                            let result = #method_call;
                            let _ = respond.send(result);
                        }, span).await
                    }
                }
            } else {
                quote! {
                    #msg_enum_name::#variant_ident { #param_pattern } => {
                        let result = #method_call;
                        let _ = respond.send(result);
                    }
                }
            }
        }
    });

    // Generate client inherent methods (no trait needed!)
    let client_inherent_methods = methods.iter().map(|(method, traced)| {
        let method_name = &method.sig.ident;
        let variant_name = to_pascal_case(&method_name.to_string());
        let variant_ident = format_ident!("{}", variant_name);

        // Get parameters with types (skip &self)
        let params: Vec<_> = method.sig.inputs.iter().skip(1).filter_map(|arg| {
            if let FnArg::Typed(pat_type) = arg {
                if let Pat::Ident(pat_ident) = &*pat_type.pat {
                    let name = &pat_ident.ident;
                    let ty = &pat_type.ty;
                    return Some((name.clone(), quote! { #ty }));
                }
            }
            None
        }).collect();

        let param_decls: Vec<_> = params.iter().map(|(name, ty)| quote! { #name: #ty }).collect();
        let param_names: Vec<_> = params.iter().map(|(name, _)| name).collect();

        // Get return type
        let return_type = match &method.sig.output {
            ReturnType::Default => quote! { () },
            ReturnType::Type(_, ty) => quote! { #ty },
        };

        // Capture span for traced methods
        let span_capture = if *traced {
            quote! { let span = ::archy::tracing::Span::current(); }
        } else {
            quote! {}
        };
        let span_field = if *traced {
            quote! { span, }
        } else {
            quote! {}
        };

        // Fire-and-forget optimization for unit returns
        if is_unit_return(&method.sig.output) {
            let msg_construction = if param_names.is_empty() && !*traced {
                quote! { #msg_enum_name::#variant_ident }
            } else {
                quote! { #msg_enum_name::#variant_ident { #(#param_names,)* #span_field } }
            };

            quote! {
                pub async fn #method_name(&self, #(#param_decls),*) -> ::std::result::Result<#return_type, ::archy::ServiceError> {
                    #span_capture
                    self.sender.send(#msg_construction).await
                        .map_err(|_| ::archy::ServiceError::ChannelClosed)?;
                    Ok(())
                }
            }
        } else {
            let msg_construction = if param_names.is_empty() {
                quote! { #msg_enum_name::#variant_ident { #span_field respond: tx } }
            } else {
                quote! { #msg_enum_name::#variant_ident { #(#param_names,)* #span_field respond: tx } }
            };

            quote! {
                pub async fn #method_name(&self, #(#param_decls),*) -> ::std::result::Result<#return_type, ::archy::ServiceError> {
                    #span_capture
                    let (tx, rx) = ::archy::tokio::sync::oneshot::channel();
                    self.sender.send(#msg_construction).await
                        .map_err(|_| ::archy::ServiceError::ChannelClosed)?;
                    rx.await.map_err(|_| ::archy::ServiceError::ServiceDropped)
                }
            }
        }
    });

    let expanded = quote! {
        // Original impl block (preserved)
        #input

        // Generated message enum
        pub enum #msg_enum_name {
            #(#msg_variants),*
        }

        // Generated client methods struct
        #[derive(Clone)]
        pub struct #methods_struct_name {
            sender: ::archy::async_channel::Sender<#msg_enum_name>,
        }

        // Implement ClientMethods trait for dependency injection
        impl ::archy::ClientMethods<#service_name> for #methods_struct_name {
            fn from_sender(sender: ::archy::async_channel::Sender<#msg_enum_name>) -> Self {
                Self { sender }
            }
        }

        // Inherent methods
        impl #methods_struct_name {
            #(#client_inherent_methods)*
        }

        // Generated Service implementation
        impl ::archy::Service for #service_name {
            type Message = #msg_enum_name;
            type ClientMethods = #methods_struct_name;

            fn create(app: &::archy::App) -> Self {
                <Self as ::archy::ServiceFactory>::create(app)
            }

            fn handle(self: ::std::sync::Arc<Self>, msg: Self::Message) -> impl ::std::future::Future<Output = ()> + Send {
                async move {
                    match msg {
                        #(#match_arms)*
                    }
                }
            }
        }
    };

    TokenStream::from(expanded)
}

fn to_pascal_case(s: &str) -> String {
    let mut result = String::new();
    let mut capitalize_next = true;
    for c in s.chars() {
        if c == '_' {
            capitalize_next = true;
        } else if capitalize_next {
            result.push(c.to_ascii_uppercase());
            capitalize_next = false;
        } else {
            result.push(c);
        }
    }
    result
}

/// Check if an attribute list contains an attribute with the given name
fn has_attribute(attrs: &[syn::Attribute], name: &str) -> bool {
    attrs.iter().any(|attr| attr.path().is_ident(name))
}

/// Check if a return type is unit () - used for fire-and-forget optimization
fn is_unit_return(output: &ReturnType) -> bool {
    match output {
        ReturnType::Default => true,
        ReturnType::Type(_, ty) => {
            if let syn::Type::Tuple(tuple) = &**ty {
                tuple.elems.is_empty()
            } else {
                false
            }
        }
    }
}
