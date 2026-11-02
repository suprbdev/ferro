use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::{
    Attribute, Data, DeriveInput, Fields, Generics, Ident, LitStr, Path, Type, Visibility,
    parse_macro_input,
};

fn field_tag(name: &str) -> u16 {
    let mut hash: u32 = 0x811c9dc5;
    for byte in name.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x01000193);
    }
    ((hash >> 16) ^ (hash & 0xffff)) as u16
}

enum Fallback {
    Required,
    Default,
    With(Path),
    Skip,
}

struct Field {
    ident: Option<Ident>,
    ty: Type,
    name: String,
    span: Span,
    fallback: Fallback,
}

impl Field {
    fn stored(&self) -> bool {
        !matches!(self.fallback, Fallback::Skip)
    }

    fn tag(&self) -> u16 {
        field_tag(&self.name)
    }
}

struct Attrs {
    rename: Option<String>,
    fallback: Fallback,
}

fn parse_attrs(attrs: &[Attribute]) -> syn::Result<Attrs> {
    let mut out = Attrs {
        rename: None,
        fallback: Fallback::Required,
    };
    for attr in attrs {
        if !attr.path().is_ident("ferro") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                let lit: LitStr = meta.value()?.parse()?;
                out.rename = Some(lit.value());
                Ok(())
            } else if meta.path.is_ident("skip") {
                out.fallback = Fallback::Skip;
                Ok(())
            } else if meta.path.is_ident("default") {
                if meta.input.peek(syn::Token![=]) {
                    let lit: LitStr = meta.value()?.parse()?;
                    out.fallback = Fallback::With(lit.parse()?);
                } else {
                    out.fallback = Fallback::Default;
                }
                Ok(())
            } else {
                Err(meta.error("unknown ferro attribute"))
            }
        })?;
    }
    Ok(out)
}

fn parse_fields(fields: &Fields) -> syn::Result<Vec<Field>> {
    let mut out = Vec::new();
    for (index, field) in fields.iter().enumerate() {
        let attrs = parse_attrs(&field.attrs)?;
        let name = attrs.rename.unwrap_or_else(|| match &field.ident {
            Some(ident) => ident.to_string(),
            None => index.to_string(),
        });
        out.push(Field {
            ident: field.ident.clone(),
            ty: field.ty.clone(),
            name,
            span: field.span(),
            fallback: attrs.fallback,
        });
    }
    Ok(out)
}

fn check_collisions<'a>(
    kind: &str,
    items: impl Iterator<Item = (&'a str, u16, Span)>,
) -> syn::Result<()> {
    let mut seen: Vec<(&str, u16, Span)> = Vec::new();
    for (name, tag, span) in items {
        if let Some((other, _, other_span)) = seen.iter().find(|(_, t, _)| *t == tag) {
            let mut err = syn::Error::new(
                span,
                format!(
                    "{kind} `{name}` has the same tag ({tag}) as {kind} `{other}`; use #[ferro(rename = \"...\")] on one of them"
                ),
            );
            err.combine(syn::Error::new(
                *other_span,
                format!("{kind} `{other}` collides with `{name}`"),
            ));
            return Err(err);
        }
        seen.push((name, tag, span));
    }
    Ok(())
}

fn check_fields(fields: &[Field]) -> syn::Result<()> {
    check_collisions(
        "field",
        fields
            .iter()
            .filter(|f| f.stored())
            .map(|f| (f.name.as_str(), f.tag(), f.span)),
    )
}

fn encode_stmts(fields: &[Field], access: &[TokenStream2]) -> TokenStream2 {
    let mut stored: Vec<(u16, &TokenStream2)> = fields
        .iter()
        .zip(access)
        .filter(|(f, _)| f.stored())
        .map(|(f, a)| (f.tag(), a))
        .collect();
    stored.sort_by_key(|(tag, _)| *tag);
    let count = stored.len() as u64;
    let writes = stored.iter().map(|(tag, a)| {
        quote! { __w.field(#tag, |__w| ::ferro::Data::encode(#a, __w))?; }
    });
    quote! {
        __w.varint(#count);
        #(#writes)*
    }
}

fn decode_stmts(fields: &[Field], vars: &[Ident]) -> (TokenStream2, Vec<TokenStream2>) {
    let mut decls = Vec::new();
    let mut arms = Vec::new();
    let mut values = Vec::new();
    for (field, var) in fields.iter().zip(vars) {
        let name = &field.name;
        let value = match &field.fallback {
            Fallback::Skip => quote! { ::std::default::Default::default() },
            Fallback::Required => {
                quote! { ::ferro::codec::required(#var, #name)? }
            }
            Fallback::Default => quote! {
                match #var {
                    ::std::option::Option::Some(__v) => __v,
                    ::std::option::Option::None => ::std::default::Default::default(),
                }
            },
            Fallback::With(path) => quote! {
                match #var {
                    ::std::option::Option::Some(__v) => __v,
                    ::std::option::Option::None => #path(),
                }
            },
        };
        if field.stored() {
            let tag = field.tag();
            decls.push(quote! { let mut #var = ::std::option::Option::None; });
            arms.push(quote! {
                #tag => #var = ::std::option::Option::Some(::ferro::Data::decode(__r)?),
            });
        }
        values.push(value);
    }
    let stmts = quote! {
        #(#decls)*
        __r.fields(|__tag, __r| {
            match __tag {
                #(#arms)*
                _ => {}
            }
            ::std::result::Result::Ok(())
        })?;
    };
    (stmts, values)
}

fn construct(
    path: TokenStream2,
    fields: &[Field],
    shape: &Fields,
    values: &[TokenStream2],
) -> TokenStream2 {
    match shape {
        Fields::Named(_) => {
            let idents = fields.iter().map(|f| f.ident.as_ref().unwrap());
            quote! { #path { #(#idents: #values),* } }
        }
        Fields::Unnamed(_) => quote! { #path(#(#values),*) },
        Fields::Unit => path,
    }
}

fn bound_generics(generics: &Generics) -> Generics {
    let mut generics = generics.clone();
    for param in generics.type_params_mut() {
        param.bounds.push(syn::parse_quote!(::ferro::Data));
    }
    generics
}

fn vars(count: usize) -> Vec<Ident> {
    (0..count).map(|i| format_ident!("__f{}", i)).collect()
}

fn is_transparent(attrs: &[Attribute]) -> syn::Result<bool> {
    let mut found = false;
    for attr in attrs {
        if !attr.path().is_ident("ferro") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("transparent") {
                found = true;
                Ok(())
            } else {
                Err(meta.error("unknown ferro attribute"))
            }
        })?;
    }
    Ok(found)
}

fn expand_transparent(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let name = &input.ident;
    let generics = bound_generics(&input.generics);
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new(
            input.span(),
            "transparent needs a struct with one field",
        ));
    };
    let fields = parse_fields(&data.fields)?;
    let [field] = fields.as_slice() else {
        return Err(syn::Error::new(
            input.span(),
            "transparent needs a struct with one field",
        ));
    };
    let access = match &field.ident {
        Some(ident) => quote! { &self.#ident },
        None => quote! { &self.0 },
    };
    let build = construct(quote! { Self }, &fields, &data.fields, &[quote! { __v }]);
    Ok(quote! {
        impl #impl_generics ::ferro::Data for #name #ty_generics #where_clause {
            fn encode(&self, __w: &mut ::ferro::codec::Encoder<'_>) -> ::ferro::Result<()> {
                ::ferro::Data::encode(#access, __w)
            }

            fn decode(__r: &mut ::ferro::codec::Decoder<'_>) -> ::ferro::Result<Self> {
                let __v = ::ferro::Data::decode(__r)?;
                ::std::result::Result::Ok(#build)
            }

            fn missing() -> ::std::option::Option<Self> {
                ::ferro::Data::missing().map(|__v| #build)
            }
        }
    })
}

fn expand_data(input: DeriveInput) -> syn::Result<TokenStream2> {
    if is_transparent(&input.attrs)? {
        return expand_transparent(&input);
    }
    let name = &input.ident;
    let generics = bound_generics(&input.generics);
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let (encode, decode) = match &input.data {
        Data::Struct(data) => {
            let fields = parse_fields(&data.fields)?;
            check_fields(&fields)?;
            let members: Vec<TokenStream2> = data
                .fields
                .iter()
                .enumerate()
                .map(|(i, f)| match &f.ident {
                    Some(ident) => quote! { &self.#ident },
                    None => {
                        let index = syn::Index::from(i);
                        quote! { &self.#index }
                    }
                })
                .collect();
            let encode = encode_stmts(&fields, &members);
            let vs = vars(fields.len());
            let (stmts, values) = decode_stmts(&fields, &vs);
            let build = construct(quote! { Self }, &fields, &data.fields, &values);
            (
                quote! { #encode ::std::result::Result::Ok(()) },
                quote! { #stmts ::std::result::Result::Ok(#build) },
            )
        }
        Data::Enum(data) => {
            let mut variant_names = Vec::new();
            for variant in &data.variants {
                let attrs = parse_attrs(&variant.attrs)?;
                variant_names.push(attrs.rename.unwrap_or_else(|| variant.ident.to_string()));
            }
            check_collisions(
                "variant",
                variant_names
                    .iter()
                    .zip(&data.variants)
                    .map(|(n, v)| (n.as_str(), field_tag(n), v.span())),
            )?;
            let mut encode_arms = Vec::new();
            let mut decode_arms = Vec::new();
            for (variant, vname) in data.variants.iter().zip(&variant_names) {
                let vident = &variant.ident;
                let vtag = field_tag(vname);
                let fields = parse_fields(&variant.fields)?;
                check_fields(&fields)?;
                let vs = vars(fields.len());
                let pattern = match &variant.fields {
                    Fields::Named(_) => {
                        let binds = fields.iter().zip(&vs).map(|(f, v)| {
                            let ident = f.ident.as_ref().unwrap();
                            if f.stored() {
                                quote! { #ident: #v }
                            } else {
                                quote! { #ident: _ }
                            }
                        });
                        quote! { Self::#vident { #(#binds),* } }
                    }
                    Fields::Unnamed(_) => {
                        let binds = fields.iter().zip(&vs).map(|(f, v)| {
                            if f.stored() {
                                quote! { #v }
                            } else {
                                quote! { _ }
                            }
                        });
                        quote! { Self::#vident(#(#binds),*) }
                    }
                    Fields::Unit => quote! { Self::#vident },
                };
                let access: Vec<TokenStream2> = vs.iter().map(|v| quote! { #v }).collect();
                let body = encode_stmts(&fields, &access);
                encode_arms.push(quote! {
                    #pattern => {
                        __w.u16(#vtag);
                        #body
                    }
                });
                let (stmts, values) = decode_stmts(&fields, &vs);
                let build = construct(quote! { Self::#vident }, &fields, &variant.fields, &values);
                decode_arms.push(quote! {
                    #vtag => {
                        #stmts
                        ::std::result::Result::Ok(#build)
                    }
                });
            }
            let encode_match = if data.variants.is_empty() {
                quote! { match *self {} }
            } else {
                quote! {
                    match self { #(#encode_arms)* }
                    ::std::result::Result::Ok(())
                }
            };
            (
                encode_match,
                quote! {
                    let __variant = __r.u16()?;
                    match __variant {
                        #(#decode_arms)*
                        __other => ::std::result::Result::Err(::ferro::Error::Decode(
                            ::std::format!("unknown variant tag {} for {}", __other, ::std::stringify!(#name))
                        )),
                    }
                },
            )
        }
        Data::Union(_) => {
            return Err(syn::Error::new(
                input.span(),
                "Data cannot be derived for unions",
            ));
        }
    };
    Ok(quote! {
        impl #impl_generics ::ferro::Data for #name #ty_generics #where_clause {
            fn encode(&self, __w: &mut ::ferro::codec::Encoder<'_>) -> ::ferro::Result<()> {
                #encode
            }

            fn decode(__r: &mut ::ferro::codec::Decoder<'_>) -> ::ferro::Result<Self> {
                #decode
            }
        }
    })
}

#[proc_macro_derive(Data, attributes(ferro))]
pub fn derive_data(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_data(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

enum IndexKind {
    Value,
    Each,
    Unique,
}

fn option_inner(ty: &Type) -> Option<&Type> {
    let Type::Path(path) = ty else {
        return None;
    };
    let segment = path.path.segments.last()?;
    if segment.ident != "Option" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    match arguments.args.first()? {
        syn::GenericArgument::Type(inner) => Some(inner),
        _ => None,
    }
}

struct Indexed {
    name: String,
    span: Span,
    fields: Vec<(Ident, Type)>,
    kind: IndexKind,
}

impl Indexed {
    fn field(&self) -> Ident {
        match self.kind {
            IndexKind::Unique => format_ident!("unique_{}", self.name),
            _ => format_ident!("by_{}", self.name),
        }
    }

    fn method(&self) -> Ident {
        format_ident!("by_{}", self.name)
    }

    fn sole(&self) -> Option<&(Ident, Type)> {
        match self.fields.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }

    fn sparse(&self) -> Option<&Type> {
        match self.kind {
            IndexKind::Unique => self.sole().and_then(|(_, ty)| option_inner(ty)),
            _ => None,
        }
    }

    fn value_ty(&self) -> TokenStream2 {
        if let Some(inner) = self.sparse() {
            return quote! { #inner };
        }
        let tys = self.fields.iter().map(|(_, ty)| ty);
        match (&self.kind, self.sole()) {
            (IndexKind::Each, Some((_, ty))) => quote! { <#ty as ::ferro::table::IndexEach>::Value },
            (_, Some((_, ty))) => quote! { #ty },
            _ => quote! { (#(#tys),*) },
        }
    }

    fn owned(&self) -> TokenStream2 {
        let clones = self
            .fields
            .iter()
            .map(|(ident, _)| quote! { ::std::clone::Clone::clone(&__r.#ident) });
        match self.sole() {
            Some(_) => quote! { #(#clones)* },
            None => quote! { (#(#clones),*) },
        }
    }

    fn optional(&self) -> TokenStream2 {
        let owned = self.owned();
        if self.sparse().is_some() {
            owned
        } else {
            quote! { ::std::option::Option::Some(#owned) }
        }
    }
}

fn field_named(fields: &syn::FieldsNamed, name: &Ident) -> syn::Result<(Ident, Type)> {
    fields
        .named
        .iter()
        .find(|field| field.ident.as_ref() == Some(name))
        .map(|field| (name.clone(), field.ty.clone()))
        .ok_or_else(|| syn::Error::new(name.span(), format!("no field named `{name}`")))
}

fn composite(attr: &Attribute, fields: &syn::FieldsNamed) -> syn::Result<Vec<(Ident, Type)>> {
    let names = attr.parse_args_with(
        syn::punctuated::Punctuated::<Ident, syn::Token![,]>::parse_terminated,
    )?;
    if names.len() < 2 {
        return Err(syn::Error::new(
            attr.span(),
            "a struct level index needs at least two fields",
        ));
    }
    names.iter().map(|name| field_named(fields, name)).collect()
}

fn expand_record(input: DeriveInput) -> syn::Result<TokenStream2> {
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new(
            input.span(),
            "Record can only be derived for structs",
        ));
    };
    let Fields::Named(named) = &data.fields else {
        return Err(syn::Error::new(
            input.span(),
            "Record requires a struct with named fields",
        ));
    };
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new(
            input.generics.span(),
            "Record does not support generics",
        ));
    }
    let name = &input.ident;
    let vis: &Visibility = &input.vis;
    let table = format_ident!("{}Table", name);

    let mut key: Option<(Ident, Type)> = None;
    let mut indexes: Vec<Indexed> = Vec::new();
    for field in &named.named {
        let ident = field.ident.clone().unwrap();
        for attr in &field.attrs {
            if attr.path().is_ident("key") {
                if key.is_some() {
                    return Err(syn::Error::new(
                        attr.span(),
                        "only one #[key] field is allowed",
                    ));
                }
                key = Some((ident.clone(), field.ty.clone()));
            } else if attr.path().is_ident("index") {
                let mut kind = IndexKind::Value;
                if matches!(attr.meta, syn::Meta::List(_)) {
                    attr.parse_nested_meta(|meta| {
                        if meta.path.is_ident("each") {
                            kind = IndexKind::Each;
                            Ok(())
                        } else {
                            Err(meta.error("expected `each`"))
                        }
                    })?;
                }
                indexes.push(Indexed {
                    name: ident.to_string(),
                    span: ident.span(),
                    fields: vec![(ident.clone(), field.ty.clone())],
                    kind,
                });
            } else if attr.path().is_ident("unique") {
                attr.meta.require_path_only()?;
                indexes.push(Indexed {
                    name: ident.to_string(),
                    span: ident.span(),
                    fields: vec![(ident.clone(), field.ty.clone())],
                    kind: IndexKind::Unique,
                });
            }
        }
    }
    for attr in &input.attrs {
        if attr.path().is_ident("unique") {
            let fields = composite(attr, named)?;
            let names: Vec<String> = fields.iter().map(|(ident, _)| ident.to_string()).collect();
            indexes.push(Indexed {
                name: names.join("_"),
                span: attr.span(),
                fields,
                kind: IndexKind::Unique,
            });
        }
    }
    let Some((key_ident, key_ty)) = key else {
        return Err(syn::Error::new(
            input.ident.span(),
            "Record requires exactly one #[key] field",
        ));
    };

    let mut table_fields: Vec<Field> = vec![Field {
        ident: Some(format_ident!("rows")),
        ty: syn::parse_quote!(::ferro::Map<#key_ty, #name>),
        name: "rows".into(),
        span: Span::call_site(),
        fallback: Fallback::Required,
    }];
    for ix in &indexes {
        let field = ix.field();
        let vt = ix.value_ty();
        let ty: Type = match ix.kind {
            IndexKind::Unique => syn::parse_quote!(::ferro::Map<#vt, #key_ty>),
            _ => syn::parse_quote!(::ferro::Map<(#vt, #key_ty), ()>),
        };
        table_fields.push(Field {
            ty,
            name: field.to_string(),
            ident: Some(field),
            span: ix.span,
            fallback: Fallback::Required,
        });
    }
    check_fields(&table_fields)?;

    let field_decls = table_fields.iter().map(|f| {
        let ident = f.ident.as_ref().unwrap();
        let ty = &f.ty;
        quote! { #ident: #ty }
    });
    let idents: Vec<&Ident> = table_fields
        .iter()
        .map(|f| f.ident.as_ref().unwrap())
        .collect();
    let access: Vec<TokenStream2> = idents.iter().map(|i| quote! { &self.#i }).collect();
    let encode = encode_stmts(&table_fields, &access);
    let vs = vars(table_fields.len());
    let (decode_ts, values) = decode_stmts(&table_fields, &vs);
    let index_vars: Vec<&Ident> = vs.iter().skip(1).collect();
    let index_idents: Vec<&Ident> = idents.iter().skip(1).copied().collect();

    let mut reindex = Vec::new();
    let mut conflicts = Vec::new();
    let mut methods = Vec::new();
    for ix in &indexes {
        let field = ix.field();
        let vt = ix.value_ty();
        let owned = ix.owned();
        let same = ix.fields.iter().map(|(ident, _)| quote! { __o.#ident == __n.#ident });
        let same = quote! { #(#same &&)* __o.#key_ident == __n.#key_ident };
        match ix.kind {
            IndexKind::Value => {
                reindex.push(quote! {
                    {
                        let __same = match (__old, __new) {
                            (::std::option::Option::Some(__o), ::std::option::Option::Some(__n)) => #same,
                            _ => false,
                        };
                        if !__same {
                            if let ::std::option::Option::Some(__r) = __old {
                                self.#field.remove(&(#owned, ::std::clone::Clone::clone(&__r.#key_ident)));
                            }
                            if let ::std::option::Option::Some(__r) = __new {
                                self.#field.insert((#owned, ::std::clone::Clone::clone(&__r.#key_ident)), ());
                            }
                        }
                    }
                });
            }
            IndexKind::Unique => {
                let optional = ix.optional();
                let index_name = &ix.name;
                reindex.push(quote! {
                    {
                        let __same = match (__old, __new) {
                            (::std::option::Option::Some(__o), ::std::option::Option::Some(__n)) => #same,
                            _ => false,
                        };
                        if !__same {
                            if let ::std::option::Option::Some(__value) = __old.and_then(|__r| #optional) {
                                self.#field.remove(&__value);
                            }
                            if let ::std::option::Option::Some(__r) = __new {
                                if let ::std::option::Option::Some(__value) = #optional {
                                    self.#field.insert(__value, ::std::clone::Clone::clone(&__r.#key_ident));
                                }
                            }
                        }
                    }
                });
                conflicts.push(quote! {
                    {
                        let __r = __row;
                        if let ::std::option::Option::Some(__value) = #optional {
                            if let ::std::option::Option::Some(__holder) = self.#field.get(&__value) {
                                if *__holder != __r.#key_ident && ::std::option::Option::Some(&*__holder) != __moving {
                                    return ::std::option::Option::Some(::ferro::table::Taken {
                                        index: #index_name,
                                        holder: ::std::clone::Clone::clone(&*__holder),
                                    });
                                }
                            }
                        }
                    }
                });
                let by = ix.method();
                let key_by = format_ident!("key_{}", by);
                methods.push(quote! {
                    pub fn #field(&self) -> &::ferro::Map<#vt, #key_ty> {
                        &self.#field
                    }

                    pub fn #by<__Q>(&self, value: &__Q) -> ::std::option::Option<::std::sync::Arc<#name>>
                    where
                        #vt: ::std::borrow::Borrow<__Q>,
                        __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                    {
                        ::ferro::table::unique_eq::<#vt, #name, __Q>(&self.rows, &self.#field, value)
                    }

                    pub fn #key_by<__Q>(&self, value: &__Q) -> ::std::option::Option<#key_ty>
                    where
                        #vt: ::std::borrow::Borrow<__Q>,
                        __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                    {
                        self.#field.get(value).map(|__key| ::std::clone::Clone::clone(&*__key))
                    }
                });
                continue;
            }
            IndexKind::Each => {
                let ident = &ix.fields[0].0;
                reindex.push(quote! {
                    {
                        let __collect = |__r: ::std::option::Option<&#name>| -> ::std::collections::BTreeSet<(#vt, #key_ty)> {
                            match __r {
                                ::std::option::Option::Some(__r) => ::ferro::table::IndexEach::values(&__r.#ident)
                                    .into_iter()
                                    .map(|__v| (__v, ::std::clone::Clone::clone(&__r.#key_ident)))
                                    .collect(),
                                ::std::option::Option::None => ::std::collections::BTreeSet::new(),
                            }
                        };
                        let __old = __collect(__old);
                        let __new = __collect(__new);
                        for __entry in __old.difference(&__new) {
                            self.#field.remove(__entry);
                        }
                        for __entry in __new.difference(&__old) {
                            self.#field.insert(::std::clone::Clone::clone(__entry), ());
                        }
                    }
                });
            }
        }
        let range = format_ident!("{}_range", field);
        let counts = format_ident!("{}_counts", field);
        let wrapped = ix.sole().and_then(|(_, ty)| option_inner(ty));
        if let (IndexKind::Value, Some(inner)) = (&ix.kind, wrapped) {
            let some = format_ident!("{}_some", field);
            methods.push(quote! {
                pub fn #some<__Q>(&self, value: &__Q) -> ::ferro::table::IndexIter<#name>
                where
                    #inner: ::std::borrow::Borrow<__Q>,
                    __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                {
                    ::ferro::table::index_eq_some::<#inner, #name, __Q>(&self.rows, &self.#field, value)
                }
            });
        }
        methods.push(quote! {
            pub fn #counts(&self) -> ::std::vec::Vec<(#vt, usize)> {
                ::ferro::table::index_counts(&self.#field)
            }

            pub fn #field<__Q>(&self, value: &__Q) -> ::ferro::table::IndexIter<#name>
            where
                #vt: ::std::borrow::Borrow<__Q>,
                __Q: ::std::cmp::Ord + ?::std::marker::Sized,
            {
                ::ferro::table::index_eq::<#vt, #name, __Q>(&self.rows, &self.#field, value)
            }

            pub fn #range<__Q, __R>(&self, range: __R) -> ::ferro::table::IndexIter<#name>
            where
                #vt: ::std::borrow::Borrow<__Q>,
                __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                __R: ::std::ops::RangeBounds<__Q>,
            {
                ::ferro::table::index_range::<#vt, #name, __Q, __R>(&self.rows, &self.#field, range)
            }
        });
    }

    Ok(quote! {
        #vis struct #table {
            #(#field_decls),*
        }

        impl ::std::clone::Clone for #table {
            fn clone(&self) -> Self {
                #table { #(#idents: ::std::clone::Clone::clone(&self.#idents)),* }
            }
        }

        impl ::std::default::Default for #table {
            fn default() -> Self {
                #table { #(#idents: ::std::default::Default::default()),* }
            }
        }

        impl ::ferro::Data for #table {
            fn encode(&self, __w: &mut ::ferro::codec::Encoder<'_>) -> ::ferro::Result<()> {
                #encode
                ::std::result::Result::Ok(())
            }

            fn decode(__r: &mut ::ferro::codec::Decoder<'_>) -> ::ferro::Result<Self> {
                #decode_ts
                let __rebuild = false #(|| #index_vars.is_none())*;
                #[allow(unused_mut)]
                let mut __table = #table { #(#idents: #values),* };
                if __rebuild {
                    #(__table.#index_idents = ::std::default::Default::default();)*
                    let __rows: ::std::vec::Vec<::std::sync::Arc<#name>> = __table.rows.values().collect();
                    for __row in __rows {
                        if let ::std::option::Option::Some(__taken) = ::ferro::TableData::<#name>::conflict(&__table, &*__row, ::std::option::Option::None) {
                            return ::std::result::Result::Err(::ferro::Error::Decode(::std::format!(
                                "two rows of {} share a value of unique index {}",
                                ::std::stringify!(#name),
                                __taken.index,
                            )));
                        }
                        ::ferro::TableData::<#name>::reindex(&mut __table, ::std::option::Option::None, ::std::option::Option::Some(&*__row));
                    }
                }
                ::std::result::Result::Ok(__table)
            }
        }

        impl ::ferro::Record for #name {
            type Key = #key_ty;
            type Table = #table;

            fn key(&self) -> #key_ty {
                ::std::clone::Clone::clone(&self.#key_ident)
            }
        }

        impl ::ferro::TableData<#name> for #table {
            fn rows(&self) -> &::ferro::Map<#key_ty, #name> {
                &self.rows
            }

            fn rows_mut(&mut self) -> &mut ::ferro::Map<#key_ty, #name> {
                &mut self.rows
            }

            fn reindex(&mut self, __old: ::std::option::Option<&#name>, __new: ::std::option::Option<&#name>) {
                #(#reindex)*
            }

            fn conflict(
                &self,
                __row: &#name,
                __moving: ::std::option::Option<&#key_ty>,
            ) -> ::std::option::Option<::ferro::table::Taken<#key_ty>> {
                #(#conflicts)*
                ::std::option::Option::None
            }
        }

        impl #table {
            #(#methods)*
        }
    })
}

#[proc_macro_derive(Record, attributes(key, index, unique, ferro))]
pub fn derive_record(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_record(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
