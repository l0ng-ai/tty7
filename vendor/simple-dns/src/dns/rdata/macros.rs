macro_rules! rr_wrapper {
    (#[doc=$doc:expr] $t:ident: $w:ident = $c:literal) => {
        #[derive(Debug, PartialEq, Eq, Hash, Clone)]
        #[doc = $doc]
        pub struct $t<'a>(pub $w<'a>);

        impl<'a> RR for $t<'a> {
            const TYPE_CODE: u16 = $c;
        }

        impl<'a> From<$w<'a>> for $t<'a> {
            fn from(value: $w<'a>) -> Self {
                $t(value)
            }
        }

        impl<'a> $t<'a> {
            /// Transforms the inner data into its owned type
            pub fn into_owned<'b>(self) -> $t<'b> {
                $t(self.0.into_owned())
            }
        }

        impl<'a> WireFormat<'a> for $t<'a> {
            const MINIMUM_LEN: usize = 0;
            fn parse(data: &mut BytesBuffer<'a>) -> crate::Result<Self>
            where
                Self: Sized,
            {
                $w::parse(data).map(|n| $t(n))
            }

            fn write_to<T: Write>(&self, out: &mut T) -> crate::Result<()> {
                self.0.write_to(out)
            }

            fn write_compressed_to<T: Write + Seek>(
                &'a self,
                out: &mut T,
                name_refs: &mut crate::lib::BTreeMap<&[crate::Label<'a>], u16>,
            ) -> crate::Result<()> {
                self.0.write_compressed_to(out, name_refs)
            }

            fn len(&self) -> usize {
                self.0.len()
            }
        }

        impl<'a> crate::lib::Deref for $t<'a> {
            type Target = $w<'a>;

            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }

        impl<'a> crate::lib::DerefMut for $t<'a> {
            fn deref_mut(&mut self) -> &mut Self::Target {
                &mut self.0
            }
        }
    };
}

macro_rules! rdata_enum {
    ($($i:tt$(<$x:lifetime>)?,)+) => {
        /// Represents the RData of each [`TYPE`]
        #[derive(Debug, Eq, PartialEq, Hash, Clone)]
        #[allow(missing_docs)]
        pub enum RData<'a> {
            $(
                $i($i$(<$x>)?),
            )+

            NULL(u16, NULL<'a>),
            Empty(TYPE)
        }

        impl<'a> WireFormat<'a> for RData<'a> {
            const MINIMUM_LEN: usize = 10;

            fn parse(data: &mut BytesBuffer<'a>) -> crate::Result<Self>
            where
                Self: Sized,
            {
                let rdatatype = data.get_u16()?.into();
                let rdatalen = data.peek_u16_in(6)? as usize;

                // OPT needs to look the ttl and class values, hence position will be advanced by OPT
                // parsing code
                if rdatatype == TYPE::OPT {
                    let mut opt_data = data.new_limited_to(rdatalen + 8)?;
                    return Ok(RData::OPT(OPT::parse(&mut opt_data)?))
                }

                data.advance(8)?;
                if rdatalen == 0 {
                    return Ok(RData::Empty(rdatatype));
                }

                let mut data = data.new_limited_to(rdatalen)?;
                parse_rdata(&mut data, rdatatype)
            }

            fn write_to<T: Write>(
                &self,
                out: &mut T,
            ) -> crate::Result<()> {
                match &self {
                    $(
                        RData::$i(data) => data.write_to(out),
                    )+

                    RData::NULL(_, data) => data.write_to(out),
                    RData::Empty(_) => { Ok(()) },
                }
            }

            fn write_compressed_to<T: Write + Seek>(
                &'a self,
                out: &mut T,
                name_refs: &mut crate::lib::BTreeMap<&[crate::Label<'a>], u16>,
            ) -> crate::Result<()> {
                match &self {
                    $(
                        RData::$i(data) => data.write_compressed_to(out, name_refs),
                    )+

                    RData::NULL(_, data) => data.write_compressed_to(out, name_refs),
                    RData::Empty(_) => { Ok(()) },
                }
            }

            fn len(&self) -> usize {
                match &self {
                    $(
                        RData::$i(data) => data.len(),
                    )+

                    RData::NULL(_, data) => data.len(),
                    RData::Empty(_) => 0,
                }
            }
        }



        impl<'a> RData<'a> {
            /// Returns the [`TYPE`] of this RData
            pub fn type_code(&self) -> TYPE {
                match self {
                    $(
                        RData::$i(_) => TYPE::$i,
                    )+

                    RData::NULL(type_code, _) => TYPE::Unknown(*type_code),
                    RData::Empty(ty) => *ty
                }
            }

            /// Transforms the inner data into its owned type
            pub fn into_owned<'b>(self) -> RData<'b> {
                match self {
                    $(
                        RData::$i(data) => RData::$i(data.into_owned()),
                    )+

                    RData::NULL(rdatatype, data) => RData::NULL(rdatatype, data.into_owned()),
                    RData::Empty(ty) => RData::Empty(ty)
                }
            }
        }

        fn parse_rdata<'a>(data: &mut BytesBuffer<'a>, rdatatype: TYPE) -> crate::Result<RData<'a>> {
            let rdata = match rdatatype {
                $(
                    TYPE::$i => RData::$i($i::parse(data)?),
                )+

                TYPE::NULL => RData::NULL(rdatatype.into(), NULL::parse(data)?),
                TYPE::Unknown(rdatatype) => RData::NULL(rdatatype, NULL::parse(data)?),
            };

            Ok(rdata)
        }


        /// Possible TYPE values in DNS Resource Records
        /// Each value is described according to its own RFC
        #[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
        #[allow(missing_docs)]
        #[non_exhaustive]
        pub enum TYPE {
            $( $i,)+

            NULL,
            Unknown(u16)
        }


        impl From<TYPE> for u16 {
            fn from(value: TYPE) -> Self {
                match value {
                    $(
                        TYPE::$i => $i::TYPE_CODE,
                    )+

                    TYPE::NULL => NULL::TYPE_CODE,
                    TYPE::Unknown(x) => x,
                }
            }
        }

        impl From<u16> for TYPE {
            fn from(value: u16) -> Self {
                match value {
                    $(
                        $i::TYPE_CODE => TYPE::$i,
                    )+

                    NULL::TYPE_CODE => TYPE::NULL,
                    v => TYPE::Unknown(v),
                }
            }
        }
    }
}

pub(crate) use rdata_enum;
pub(crate) use rr_wrapper;
