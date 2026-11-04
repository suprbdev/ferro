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

fn visit_stmts(fields: &[Field]) -> TokenStream2 {
    let stored: Vec<&Field> = fields.iter().filter(|f| f.stored()).collect();
    let seen: Vec<Ident> = (0..stored.len())
        .map(|i| format_ident!("__seen{}", i))
        .collect();
    let arms = stored.iter().zip(&seen).map(|(field, seen)| {
        let tag = field.tag();
        let ty = &field.ty;
        quote! {
            #tag => {
                #seen = true;
                <#ty as ::ferro::Data>::visit(__r)?;
            }
        }
    });
    let required = stored
        .iter()
        .zip(&seen)
        .filter(|(field, _)| matches!(field.fallback, Fallback::Required))
        .map(|(field, seen)| {
            let ty = &field.ty;
            let name = &field.name;
            quote! {
                if !#seen && <#ty as ::ferro::Data>::missing().is_none() {
                    return ::std::result::Result::Err(::ferro::Error::MissingField(#name));
                }
            }
        });
    quote! {
        #(let mut #seen = false;)*
        __r.fields(|__tag, __r| {
            match __tag {
                #(#arms)*
                _ => {}
            }
            ::std::result::Result::Ok(())
        })?;
        #(#required)*
    }
}

fn describe_stmts(fields: &[Field]) -> TokenStream2 {
    let mut stored: Vec<&Field> = fields.iter().filter(|f| f.stored()).collect();
    stored.sort_by(|a, b| (a.tag(), &a.name).cmp(&(b.tag(), &b.name)));
    let count = stored.len() as u64;
    let parts = stored.iter().map(|field| {
        let name = &field.name;
        let ty = &field.ty;
        quote! {
            __d.word(#name);
            __d.of::<#ty>();
        }
    });
    quote! {
        __d.count(#count);
        #(#parts)*
    }
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
    let ty = &field.ty;
    Ok(quote! {
        impl #impl_generics ::ferro::Data for #name #ty_generics #where_clause {
            fn encode(&self, __w: &mut ::ferro::codec::Encoder<'_>) -> ::ferro::Result<()> {
                ::ferro::Data::encode(#access, __w)
            }

            fn decode(__r: &mut ::ferro::codec::Decoder<'_>) -> ::ferro::Result<Self> {
                let __v = ::ferro::Data::decode(__r)?;
                ::std::result::Result::Ok(#build)
            }

            fn visit(__r: &mut ::ferro::codec::Decoder<'_>) -> ::ferro::Result<()> {
                <#ty as ::ferro::Data>::visit(__r)
            }

            fn missing() -> ::std::option::Option<Self> {
                ::ferro::Data::missing().map(|__v| #build)
            }

            fn describe(__d: &mut ::ferro::codec::Description) {
                __d.named::<Self>(|__d| __d.of::<#ty>());
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
    let (encode, decode, visit, describe) = match &input.data {
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
            let visit = visit_stmts(&fields);
            let described = describe_stmts(&fields);
            (
                quote! { #encode ::std::result::Result::Ok(()) },
                quote! { #stmts ::std::result::Result::Ok(#build) },
                quote! { #visit ::std::result::Result::Ok(()) },
                quote! {
                    __d.word("struct");
                    #described
                },
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
            let mut visit_arms = Vec::new();
            let mut described = Vec::new();
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
                let visit = visit_stmts(&fields);
                visit_arms.push(quote! {
                    #vtag => {
                        #visit
                        ::std::result::Result::Ok(())
                    }
                });
                let fields_described = describe_stmts(&fields);
                described.push((
                    vtag,
                    vname.clone(),
                    quote! {
                        __d.word(#vname);
                        #fields_described
                    },
                ));
            }
            described.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
            let variant_count = described.len() as u64;
            let variants_described = described.iter().map(|(_, _, tokens)| tokens);
            let encode_match = if data.variants.is_empty() {
                quote! { match *self {} }
            } else {
                quote! {
                    match self { #(#encode_arms)* }
                    ::std::result::Result::Ok(())
                }
            };
            let unknown = quote! {
                __other => ::std::result::Result::Err(::ferro::Error::Decode(
                    ::std::format!("unknown variant tag {} for {}", __other, ::std::stringify!(#name))
                )),
            };
            (
                encode_match,
                quote! {
                    let __variant = __r.u16()?;
                    match __variant {
                        #(#decode_arms)*
                        #unknown
                    }
                },
                quote! {
                    let __variant = __r.u16()?;
                    match __variant {
                        #(#visit_arms)*
                        #unknown
                    }
                },
                quote! {
                    __d.word("enum");
                    __d.count(#variant_count);
                    #(#variants_described)*
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

            fn visit(__r: &mut ::ferro::codec::Decoder<'_>) -> ::ferro::Result<()> {
                #visit
            }

            fn describe(__d: &mut ::ferro::codec::Description) {
                __d.named::<Self>(|__d| {
                    #describe
                });
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
    cover: Vec<(Ident, Type)>,
}

const MAX_STORED: usize = 12;

impl Indexed {
    fn indexes(&self, ident: &Ident) -> Option<usize> {
        self.fields.iter().position(|(field, _)| field == ident)
    }

    fn stored(&self) -> Vec<&(Ident, Type)> {
        self.cover
            .iter()
            .filter(|(ident, _)| self.indexes(ident).is_none())
            .collect()
    }

    fn stores_cover(&self) -> bool {
        !self.stored().is_empty()
    }

    fn stored_ty(&self) -> TokenStream2 {
        let tys = self.stored().into_iter().map(|(_, ty)| ty);
        quote! { (#(#tys,)*) }
    }

    fn cover_ty(&self) -> TokenStream2 {
        let tys: Vec<&Type> = self.cover.iter().map(|(_, ty)| ty).collect();
        match tys.as_slice() {
            [only] => quote! { #only },
            _ => quote! { (#(#tys),*) },
        }
    }

    fn stored_of(&self) -> TokenStream2 {
        let clones = self
            .stored()
            .into_iter()
            .map(|(ident, _)| quote! { ::std::clone::Clone::clone(&__r.#ident) });
        quote! { (#(#clones,)*) }
    }

    fn same_cover(&self) -> TokenStream2 {
        let same = self
            .stored()
            .into_iter()
            .map(|(ident, _)| quote! { ::ferro::table::same_stored(&__o.#ident, &__n.#ident) });
        quote! { true #(&& #same)* }
    }

    fn kind_word(&self) -> &'static str {
        let composite = self.fields.len() > 1;
        let option = self
            .sole()
            .is_some_and(|(_, ty)| option_inner(ty).is_some());
        match self.kind {
            IndexKind::Value if composite => "composite",
            IndexKind::Value if option => "option",
            IndexKind::Value => "value",
            IndexKind::Each => "each",
            IndexKind::Unique if composite => "unique composite",
            IndexKind::Unique if option => "unique option",
            IndexKind::Unique => "unique",
        }
    }

    fn fingerprint(&self, key_ty: &Type, stored_name: &dyn Fn(&Ident) -> String) -> TokenStream2 {
        let kind = self.kind_word();
        let described = |fields: Vec<&(Ident, Type)>| {
            let count = fields.len() as u64;
            let parts = fields.into_iter().map(|(ident, ty)| {
                let name = stored_name(ident);
                quote! {
                    __d.word(#name);
                    __d.of::<#ty>();
                }
            });
            quote! {
                __d.count(#count);
                #(#parts)*
            }
        };
        let keyed = described(self.fields.iter().collect());
        let covered = described(self.stored());
        quote! {
            ::ferro::table::index_fingerprint(|__d| {
                __d.word(#kind);
                __d.of::<#key_ty>();
                #keyed
                #covered
            })
        }
    }

    fn projected(&self) -> TokenStream2 {
        let mut parts = Vec::new();
        let mut slot = 0;
        for (ident, _) in &self.cover {
            parts.push(match self.indexes(ident) {
                Some(_) if self.fields.len() == 1 => {
                    quote! { ::std::clone::Clone::clone(__value) }
                }
                Some(position) => {
                    let position = syn::Index::from(position);
                    quote! { ::std::clone::Clone::clone(&__value.#position) }
                }
                None => {
                    let position = syn::Index::from(slot);
                    slot += 1;
                    quote! { __stored.#position }
                }
            });
        }
        match parts.as_slice() {
            [only] => quote! { #only },
            _ => quote! { (#(#parts),*) },
        }
    }

    fn project_fn(&self) -> Ident {
        format_ident!("__{}_cover", self.field())
    }

    fn project_path(&self) -> TokenStream2 {
        if self.cover.is_empty() {
            let vt = self.value_ty();
            quote! { ::ferro::table::plain::<#vt> }
        } else {
            let project = self.project_fn();
            quote! { Self::#project }
        }
    }

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
            (IndexKind::Each, Some((_, ty))) => {
                quote! { <#ty as ::ferro::table::IndexEach>::Value }
            }
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

fn cover_list(
    meta: &syn::meta::ParseNestedMeta<'_>,
    fields: &syn::FieldsNamed,
) -> syn::Result<Vec<(Ident, Type)>> {
    let content;
    syn::parenthesized!(content in meta.input);
    let names = syn::punctuated::Punctuated::<Ident, syn::Token![,]>::parse_terminated(&content)?;
    if names.is_empty() {
        return Err(meta.error("cover needs at least one field"));
    }
    names.iter().map(|name| field_named(fields, name)).collect()
}

fn is_cover(meta: &syn::meta::ParseNestedMeta<'_>) -> bool {
    meta.path.is_ident("cover") && meta.input.peek(syn::token::Paren)
}

type Composite = (Vec<(Ident, Type)>, Vec<(Ident, Type)>);

fn composite(attr: &Attribute, fields: &syn::FieldsNamed) -> syn::Result<Composite> {
    let mut names = Vec::new();
    let mut cover = None;
    attr.parse_nested_meta(|meta| {
        if is_cover(&meta) {
            if cover.is_some() {
                return Err(meta.error("an index takes one cover list"));
            }
            cover = Some(cover_list(&meta, fields)?);
            return Ok(());
        }
        names.push(meta.path.require_ident()?.clone());
        Ok(())
    })?;
    if names.len() < 2 {
        return Err(syn::Error::new(
            attr.span(),
            "a struct level index needs at least two fields",
        ));
    }
    let indexed = names
        .iter()
        .map(|name| field_named(fields, name))
        .collect::<syn::Result<_>>()?;
    Ok((indexed, cover.unwrap_or_default()))
}

fn check_cover(ix: &Indexed, key: &Ident, skipped: &[Ident]) -> syn::Result<()> {
    for (at, (ident, _)) in ix.cover.iter().enumerate() {
        let fail = |message: String| Err(syn::Error::new(ident.span(), message));
        if ix.cover[..at].iter().any(|(other, _)| other == ident) {
            return fail(format!("`{ident}` is covered twice"));
        }
        if ident == key {
            return fail(format!(
                "`{ident}` is the key, which every index entry already holds"
            ));
        }
        if skipped.contains(ident) {
            return fail(format!("`{ident}` is not stored, so it cannot be covered"));
        }
        if ix.indexes(ident).is_some() {
            match ix.kind {
                IndexKind::Value => {}
                IndexKind::Each => {
                    return fail(format!(
                        "an each index holds one element of `{ident}`, so it cannot cover `{ident}`"
                    ));
                }
                IndexKind::Unique => {
                    return fail(format!(
                        "a unique index is looked up by `{ident}`, so it cannot cover `{ident}`"
                    ));
                }
            }
        }
    }
    if ix.stored().len() > MAX_STORED {
        return Err(syn::Error::new(
            ix.span,
            format!("a cover list stores at most {MAX_STORED} fields"),
        ));
    }
    Ok(())
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
                let mut cover = Vec::new();
                if matches!(attr.meta, syn::Meta::List(_)) {
                    attr.parse_nested_meta(|meta| {
                        if meta.path.is_ident("each") {
                            kind = IndexKind::Each;
                            Ok(())
                        } else if is_cover(&meta) {
                            cover = cover_list(&meta, named)?;
                            Ok(())
                        } else {
                            Err(meta.error("expected `each` or `cover(...)`"))
                        }
                    })?;
                }
                indexes.push(Indexed {
                    name: ident.to_string(),
                    span: ident.span(),
                    fields: vec![(ident.clone(), field.ty.clone())],
                    kind,
                    cover,
                });
            } else if attr.path().is_ident("unique") {
                let mut cover = Vec::new();
                if matches!(attr.meta, syn::Meta::List(_)) {
                    attr.parse_nested_meta(|meta| {
                        if is_cover(&meta) {
                            cover = cover_list(&meta, named)?;
                            Ok(())
                        } else {
                            Err(meta.error("expected `cover(...)`"))
                        }
                    })?;
                } else {
                    attr.meta.require_path_only()?;
                }
                indexes.push(Indexed {
                    name: ident.to_string(),
                    span: ident.span(),
                    fields: vec![(ident.clone(), field.ty.clone())],
                    kind: IndexKind::Unique,
                    cover,
                });
            }
        }
    }
    for attr in &input.attrs {
        let kind = if attr.path().is_ident("unique") {
            IndexKind::Unique
        } else if attr.path().is_ident("index") {
            IndexKind::Value
        } else {
            continue;
        };
        let (fields, cover) = composite(attr, named)?;
        let names: Vec<String> = fields.iter().map(|(ident, _)| ident.to_string()).collect();
        indexes.push(Indexed {
            name: names.join("_"),
            span: attr.span(),
            fields,
            kind,
            cover,
        });
    }
    let Some((key_ident, key_ty)) = key else {
        return Err(syn::Error::new(
            input.ident.span(),
            "Record requires exactly one #[key] field",
        ));
    };
    let mut skipped = Vec::new();
    let mut stored_as = Vec::new();
    for field in &named.named {
        let attrs = parse_attrs(&field.attrs)?;
        let Some(ident) = &field.ident else {
            continue;
        };
        if matches!(attrs.fallback, Fallback::Skip) {
            skipped.push(ident.clone());
        }
        stored_as.push((
            ident.clone(),
            attrs.rename.unwrap_or_else(|| ident.to_string()),
        ));
    }
    for ix in &indexes {
        check_cover(ix, &key_ident, &skipped)?;
    }

    let stored_name = |ident: &Ident| -> String {
        stored_as
            .iter()
            .find(|(field, _)| field == ident)
            .map_or_else(|| ident.to_string(), |(_, name)| name.clone())
    };
    let rows_ty: Type = syn::parse_quote!(::ferro::Map<#key_ty, #name>);
    let map_tys: Vec<Type> = indexes
        .iter()
        .map(|ix| {
            let vt = ix.value_ty();
            let st = ix.stored_ty();
            match ix.kind {
                IndexKind::Unique if ix.stores_cover() => {
                    syn::parse_quote!(::ferro::Map<#vt, (#key_ty, #st)>)
                }
                IndexKind::Unique => syn::parse_quote!(::ferro::Map<#vt, #key_ty>),
                _ => syn::parse_quote!(::ferro::Map<(#vt, #key_ty), #st>),
            }
        })
        .collect();
    let fingerprints: Vec<TokenStream2> = indexes
        .iter()
        .map(|ix| ix.fingerprint(&key_ty, &stored_name))
        .collect();
    let index_fields: Vec<Ident> = indexes.iter().map(Indexed::field).collect();

    let mut reindex = Vec::new();
    let mut clashes = Vec::new();
    let mut methods = Vec::new();
    for (slot, ix) in indexes.iter().enumerate() {
        let field = ix.field();
        let vt = ix.value_ty();
        let st = ix.stored_ty();
        let ct = ix.cover_ty();
        let owned = ix.owned();
        let stored_of = ix.stored_of();
        let same_cover = ix.same_cover();
        let project = ix.project_path();
        let same = ix
            .fields
            .iter()
            .map(|(ident, _)| quote! { __o.#ident == __n.#ident });
        let same = quote! { #(#same &&)* __o.#key_ident == __n.#key_ident };
        if !ix.cover.is_empty() {
            let project_fn = ix.project_fn();
            let projected = ix.projected();
            let signature = match ix.kind {
                IndexKind::Unique => quote! { __stored: #st },
                _ => quote! { __value: &#vt, __stored: #st },
            };
            methods.push(quote! {
                fn #project_fn(#signature) -> #ct {
                    #projected
                }
            });
        }
        match ix.kind {
            IndexKind::Value if ix.stores_cover() => {
                reindex.push(quote! {
                    {
                        let (__same, __same_cover) = match (__old, __new) {
                            (::std::option::Option::Some(__o), ::std::option::Option::Some(__n)) => (#same, #same_cover),
                            _ => (false, false),
                        };
                        if !__same {
                            if let ::std::option::Option::Some(__r) = __old {
                                self.#field.remove(&(#owned, ::std::clone::Clone::clone(&__r.#key_ident)));
                            }
                        }
                        if !__same || !__same_cover {
                            if let ::std::option::Option::Some(__r) = __new {
                                self.#field.insert((#owned, ::std::clone::Clone::clone(&__r.#key_ident)), #stored_of);
                            }
                        }
                    }
                });
            }
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
                let (entry, held, entry_ty) = if ix.stores_cover() {
                    (
                        quote! { (::std::clone::Clone::clone(&__r.#key_ident), #stored_of) },
                        quote! { &__holder.0 },
                        quote! { (#key_ty, #st) },
                    )
                } else {
                    (
                        quote! { ::std::clone::Clone::clone(&__r.#key_ident) },
                        quote! { &*__holder },
                        quote! { #key_ty },
                    )
                };
                reindex.push(quote! {
                    {
                        let (__same, __same_cover) = match (__old, __new) {
                            (::std::option::Option::Some(__o), ::std::option::Option::Some(__n)) => (#same, #same_cover),
                            _ => (false, false),
                        };
                        if !__same {
                            if let ::std::option::Option::Some(__value) = __old.and_then(|__r| #optional) {
                                self.#field.remove(&__value);
                            }
                        }
                        if !__same || !__same_cover {
                            if let ::std::option::Option::Some(__r) = __new {
                                if let ::std::option::Option::Some(__value) = #optional {
                                    self.#field.insert(__value, #entry);
                                }
                            }
                        }
                    }
                });
                clashes.push((slot, quote! {
                    {
                        let __r = __row;
                        if let ::std::option::Option::Some(__value) = #optional {
                            if let ::std::option::Option::Some(__holder) = self.#field.get(&__value) {
                                let __held: &#key_ty = #held;
                                if *__held != __r.#key_ident && ::std::option::Option::Some(__held) != __moving {
                                    return ::std::option::Option::Some(::ferro::table::Taken {
                                        index: #index_name,
                                        holder: ::std::clone::Clone::clone(__held),
                                    });
                                }
                            }
                        }
                    }
                }));
                let by = ix.method();
                let key_by = format_ident!("key_{}", by);
                methods.push(quote! {
                    pub fn #field(&self) -> &::ferro::Map<#vt, #entry_ty> {
                        &self.#field
                    }

                    pub fn #key_by<__Q>(&self, value: &__Q) -> ::std::option::Option<#key_ty>
                    where
                        #vt: ::std::borrow::Borrow<__Q>,
                        __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                    {
                        self.#field.get(value).map(|__holder| {
                            let __held: &#key_ty = #held;
                            ::std::clone::Clone::clone(__held)
                        })
                    }
                });
                if ix.stores_cover() {
                    let covered_by = format_ident!("covered_{}", by);
                    let project_fn = ix.project_fn();
                    methods.push(quote! {
                        pub fn #by<__Q>(&self, value: &__Q) -> ::std::option::Option<::ferro::Val<#name>>
                        where
                            #vt: ::std::borrow::Borrow<__Q>,
                            __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                        {
                            let __holder = self.#field.get(value)?;
                            self.rows.get::<#key_ty>(&__holder.0)
                        }

                        pub fn #covered_by<__Q>(&self, value: &__Q) -> ::std::option::Option<(#key_ty, #ct)>
                        where
                            #vt: ::std::borrow::Borrow<__Q>,
                            __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                        {
                            self.#field.get(value).map(|__holder| {
                                let (__key, __stored) = ::ferro::Val::into_owned(__holder);
                                (__key, Self::#project_fn(__stored))
                            })
                        }
                    });
                } else {
                    methods.push(quote! {
                        pub fn #by<__Q>(&self, value: &__Q) -> ::std::option::Option<::ferro::Val<#name>>
                        where
                            #vt: ::std::borrow::Borrow<__Q>,
                            __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                        {
                            ::ferro::table::unique_eq::<#vt, #name, __Q>(&self.rows, &self.#field, value)
                        }
                    });
                }
                continue;
            }
            IndexKind::Each if ix.stores_cover() => {
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
                        let __same_cover = match (__old, __new) {
                            (::std::option::Option::Some(__o), ::std::option::Option::Some(__n)) => #same_cover,
                            _ => false,
                        };
                        let __before = __collect(__old);
                        let __after = __collect(__new);
                        for __entry in __before.difference(&__after) {
                            self.#field.remove(__entry);
                        }
                        if let ::std::option::Option::Some(__r) = __new {
                            let __stored = #stored_of;
                            for __entry in &__after {
                                if !__same_cover || !__before.contains(__entry) {
                                    self.#field.insert(::std::clone::Clone::clone(__entry), ::std::clone::Clone::clone(&__stored));
                                }
                            }
                        }
                    }
                });
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
        let after = format_ident!("{}_after", field);
        if let (IndexKind::Value, None) = (&ix.kind, ix.sole()) {
            let prefix = format_ident!("{}_prefix", field);
            let first = &ix.fields[0].1;
            methods.push(quote! {
                pub fn #prefix<__Q>(&self, value: &__Q) -> ::ferro::table::IndexIter<#name, #ct>
                where
                    #first: ::std::borrow::Borrow<__Q>,
                    __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                {
                    ::ferro::table::index_where::<#vt, #name, #st, #ct>(
                        &self.rows,
                        &self.#field,
                        #project,
                        |__entry| ::std::borrow::Borrow::<__Q>::borrow(&__entry.0) < value,
                        |__entry| ::std::borrow::Borrow::<__Q>::borrow(&__entry.0) <= value,
                    )
                }
            });
        }
        let wrapped = ix.sole().and_then(|(_, ty)| option_inner(ty));
        if let (IndexKind::Value, Some(inner)) = (&ix.kind, wrapped) {
            let some = format_ident!("{}_some", field);
            let some_after = format_ident!("{}_some_after", field);
            methods.push(quote! {
                pub fn #some<__Q>(&self, value: &__Q) -> ::ferro::table::IndexIter<#name, #ct>
                where
                    #inner: ::std::borrow::Borrow<__Q>,
                    __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                {
                    ::ferro::table::index_eq_some::<#inner, #name, #st, #ct, __Q>(&self.rows, &self.#field, #project, value)
                }

                pub fn #some_after<__Q, __A>(&self, value: &__Q, after: &__A) -> ::ferro::table::IndexIter<#name, #ct>
                where
                    #inner: ::std::borrow::Borrow<__Q>,
                    #key_ty: ::std::borrow::Borrow<__A>,
                    __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                    __A: ::std::cmp::Ord + ?::std::marker::Sized,
                {
                    ::ferro::table::index_after_some::<#inner, #name, #st, #ct, __Q, __A>(&self.rows, &self.#field, #project, value, after)
                }
            });
        }
        methods.push(quote! {
            pub fn #counts(&self) -> ::std::vec::Vec<(#vt, usize)> {
                ::ferro::table::index_counts(&self.#field)
            }

            pub fn #field<__Q>(&self, value: &__Q) -> ::ferro::table::IndexIter<#name, #ct>
            where
                #vt: ::std::borrow::Borrow<__Q>,
                __Q: ::std::cmp::Ord + ?::std::marker::Sized,
            {
                ::ferro::table::index_eq::<#vt, #name, #st, #ct, __Q>(&self.rows, &self.#field, #project, value)
            }

            pub fn #after<__Q, __A>(&self, value: &__Q, after: &__A) -> ::ferro::table::IndexIter<#name, #ct>
            where
                #vt: ::std::borrow::Borrow<__Q>,
                #key_ty: ::std::borrow::Borrow<__A>,
                __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                __A: ::std::cmp::Ord + ?::std::marker::Sized,
            {
                ::ferro::table::index_after::<#vt, #name, #st, #ct, __Q, __A>(&self.rows, &self.#field, #project, value, after)
            }

            pub fn #range<__Q, __R>(&self, range: __R) -> ::ferro::table::IndexIter<#name, #ct>
            where
                #vt: ::std::borrow::Borrow<__Q>,
                __Q: ::std::cmp::Ord + ?::std::marker::Sized,
                __R: ::std::ops::RangeBounds<__Q>,
            {
                ::ferro::table::index_range::<#vt, #name, #st, #ct, __Q, __R>(&self.rows, &self.#field, #project, range)
            }
        });
    }

    let rows_tag = field_tag("rows");
    let index_tag = field_tag("indexes");
    let count = indexes.len();
    let total = count as u64 + 1;
    let slots: Vec<usize> = (0..count).collect();
    let vars: Vec<Ident> = (0..count).map(|i| format_ident!("__index{}", i)).collect();
    let reindex_fns: Vec<Ident> = index_fields
        .iter()
        .map(|field| format_ident!("__reindex_{}", field))
        .collect();
    let clash_slots: Vec<usize> = clashes.iter().map(|(slot, _)| *slot).collect();
    let clash_fns: Vec<Ident> = clash_slots
        .iter()
        .map(|slot| format_ident!("__clash_{}", index_fields[*slot]))
        .collect();
    let clash_blocks = clashes.iter().map(|(_, block)| block);

    let encode_rows = quote! {
        __w.field(#rows_tag, |__w| ::ferro::Data::encode(&self.rows, __w))?;
    };
    let encode_indexes = if count == 0 {
        quote! {}
    } else {
        quote! {
            let __fingerprints = Self::__fingerprints();
            for &__slot in __fingerprints.order() {
                let __fingerprint = __fingerprints.get(__slot);
                match __slot {
                    #(#slots => __w.field(#index_tag, |__w| ::ferro::table::encode_index(__fingerprint, &self.#index_fields, __w))?,)*
                    _ => {}
                }
            }
        }
    };
    let (encode_first, encode_last) = if rows_tag < index_tag {
        (encode_rows, encode_indexes)
    } else {
        (encode_indexes, encode_rows)
    };
    let fingerprints_of = if count == 0 {
        quote! {}
    } else {
        quote! { let __fingerprints = Self::__fingerprints(); }
    };
    let decode_arm = if count == 0 {
        quote! {}
    } else {
        quote! {
            #index_tag => match __fingerprints.slot(__r) {
                #(::std::option::Option::Some(#slots) => #vars = ::std::option::Option::Some(::ferro::Data::decode(__r)?),)*
                _ => {}
            },
        }
    };
    let visit_arm = if count == 0 {
        quote! {}
    } else {
        quote! {
            #index_tag => match __fingerprints.slot(__r) {
                #(::std::option::Option::Some(#slots) => <#map_tys as ::ferro::Data>::visit(__r)?,)*
                _ => {}
            },
        }
    };
    let stale = if count == 0 {
        quote! {}
    } else {
        quote! { let __stale = [#(#vars.is_none()),*]; }
    };
    let rebuild = if count == 0 {
        quote! {}
    } else {
        quote! {
            if __stale.contains(&true) {
                let __rows = ::ferro::table::stored_rows::<#name>(&__table.rows)?;
                for __row in __rows {
                    #(
                        if __stale[#clash_slots] {
                            if let ::std::option::Option::Some(__taken) = __table.#clash_fns(&*__row, ::std::option::Option::None) {
                                return ::std::result::Result::Err(::ferro::Error::Decode(::std::format!(
                                    "two rows of {} share a value of unique index {}",
                                    ::std::stringify!(#name),
                                    __taken.index,
                                )));
                            }
                        }
                    )*
                    #(
                        if __stale[#slots] {
                            __table.#reindex_fns(::std::option::Option::None, ::std::option::Option::Some(&*__row));
                        }
                    )*
                }
            }
        }
    };
    let fingerprints_fn = if count == 0 {
        quote! {}
    } else {
        quote! {
            fn __fingerprints() -> &'static ::ferro::table::Fingerprints<#count> {
                static __FINGERPRINTS: ::std::sync::OnceLock<::ferro::table::Fingerprints<#count>> =
                    ::std::sync::OnceLock::new();
                __FINGERPRINTS.get_or_init(|| {
                    ::ferro::table::Fingerprints::new(::std::stringify!(#name), [#(#fingerprints),*])
                })
            }
        }
    };

    Ok(quote! {
        #vis struct #table {
            rows: #rows_ty,
            #(#index_fields: #map_tys),*
        }

        impl ::std::clone::Clone for #table {
            fn clone(&self) -> Self {
                #table {
                    rows: ::std::clone::Clone::clone(&self.rows),
                    #(#index_fields: ::std::clone::Clone::clone(&self.#index_fields)),*
                }
            }
        }

        impl ::std::default::Default for #table {
            fn default() -> Self {
                #table {
                    rows: ::std::default::Default::default(),
                    #(#index_fields: ::std::default::Default::default()),*
                }
            }
        }

        impl ::ferro::Data for #table {
            fn encode(&self, __w: &mut ::ferro::codec::Encoder<'_>) -> ::ferro::Result<()> {
                __w.varint(#total);
                #encode_first
                #encode_last
                ::std::result::Result::Ok(())
            }

            fn decode(__r: &mut ::ferro::codec::Decoder<'_>) -> ::ferro::Result<Self> {
                #fingerprints_of
                let mut __rows: ::std::option::Option<#rows_ty> = ::std::option::Option::None;
                #(let mut #vars: ::std::option::Option<#map_tys> = ::std::option::Option::None;)*
                __r.fields(|__tag, __r| {
                    match __tag {
                        #rows_tag => __rows = ::std::option::Option::Some(::ferro::Data::decode(__r)?),
                        #decode_arm
                        _ => {}
                    }
                    ::std::result::Result::Ok(())
                })?;
                #stale
                #[allow(unused_mut)]
                let mut __table = #table {
                    rows: ::ferro::codec::required(__rows, "rows")?,
                    #(#index_fields: ::std::option::Option::unwrap_or_default(#vars)),*
                };
                #rebuild
                ::std::result::Result::Ok(__table)
            }

            fn visit(__r: &mut ::ferro::codec::Decoder<'_>) -> ::ferro::Result<()> {
                #fingerprints_of
                __r.fields(|__tag, __r| {
                    match __tag {
                        #rows_tag => <#rows_ty as ::ferro::Data>::visit(__r)?,
                        #visit_arm
                        _ => {}
                    }
                    ::std::result::Result::Ok(())
                })?;
                ::std::result::Result::Ok(())
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
                #(self.#reindex_fns(__old, __new);)*
            }

            fn conflict(
                &self,
                __row: &#name,
                __moving: ::std::option::Option<&#key_ty>,
            ) -> ::std::option::Option<::ferro::table::Taken<#key_ty>> {
                #(
                    if let ::std::option::Option::Some(__taken) = self.#clash_fns(__row, __moving) {
                        return ::std::option::Option::Some(__taken);
                    }
                )*
                ::std::option::Option::None
            }
        }

        impl #table {
            #fingerprints_fn

            #(
                #[inline]
                fn #reindex_fns(&mut self, __old: ::std::option::Option<&#name>, __new: ::std::option::Option<&#name>) {
                    #reindex
                }
            )*

            #(
                #[inline]
                fn #clash_fns(
                    &self,
                    __row: &#name,
                    __moving: ::std::option::Option<&#key_ty>,
                ) -> ::std::option::Option<::ferro::table::Taken<#key_ty>> {
                    #clash_blocks
                    ::std::option::Option::None
                }
            )*

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
